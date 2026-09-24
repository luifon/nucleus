//! Usage accounting (ADR-034): token and estimated-dollar usage of every
//! Claude Code and Codex session on the machine, per project, per model,
//! and — for Nucleus-spawned sessions — per agent and reminder.
//!
//! Core owns `memory/usage.db` (ADR-020). The single writer is
//! [`refresh`], reached through `nucleus usage refresh` (the dashboard
//! spawns it) and the distiller's daily pass; a lock file serializes runs.
//! The dashboard opens the DB read-only.
//!
//! The store keeps aggregates permanently. Claude Code deletes transcripts
//! after about 30 days, so a refresh at least that often keeps the record
//! complete; the first refresh backfills whatever still exists.
//!
//! Incremental: each source file's read offset is stored with the records
//! it produced (one transaction), so a refresh reads only appended bytes.
//! Every write is keyed and idempotent; `--full` re-reads everything and
//! converges on the same rows.

pub mod attribution;
pub mod claude;
pub mod codex;
pub mod pricing;
pub mod project;
pub mod query;
pub mod reconcile;
pub mod records;
pub mod schema;
pub mod store;

use crate::config::UsageConfig;
use anyhow::{Context, Result, bail};
use records::{Record, Vendor};
use sqlx::SqlitePool;
use std::io::{BufRead, Seek};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const DB_PATH: &str = "memory/usage.db";
pub const LOCK_PATH: &str = "memory/usage-refresh.lock";
/// A lock file untouched for this long belongs to a dead refresh.
pub const LOCK_STALE: Duration = Duration::from_secs(300);

/// Open for writing (the refresh). Applies migrations.
pub async fn open(workspace_root: &Path) -> Result<SqlitePool> {
    let pool = crate::db::open(&workspace_root.join(DB_PATH)).await?;
    crate::migrate::migrate(&pool, schema::MIGRATIONS).await?;
    Ok(pool)
}

/// Open for reading (the dashboard). `None` when no refresh has run yet.
pub async fn open_read_only(workspace_root: &Path) -> Result<Option<SqlitePool>> {
    let path = workspace_root.join(DB_PATH);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(crate::db::open_read_only(&path).await?))
}

/// Whether a refresh holds the lock right now.
pub fn refresh_running(workspace_root: &Path) -> bool {
    let lock = workspace_root.join(LOCK_PATH);
    std::fs::metadata(&lock)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .is_some_and(|age| age < LOCK_STALE)
}

/// Lock file held for the refresh; touched after every file (heartbeat) and
/// removed on drop.
struct RefreshLock {
    path: PathBuf,
}

impl RefreshLock {
    fn acquire(workspace_root: &Path) -> Result<Self> {
        let path = workspace_root.join(LOCK_PATH);
        for _ in 0..2 {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut f) => {
                    use std::io::Write;
                    let _ = writeln!(f, "{}", std::process::id());
                    return Ok(Self { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    if refresh_running(workspace_root) {
                        bail!("a usage refresh is already running ({} is fresh)", path.display());
                    }
                    tracing::warn!("usage: reclaiming stale refresh lock {}", path.display());
                    let _ = std::fs::remove_file(&path);
                }
                Err(e) => return Err(e).with_context(|| format!("creating {}", path.display())),
            }
        }
        bail!("could not acquire {}", path.display())
    }

    fn heartbeat(&self) {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&self.path) {
            let _ = f.set_modified(SystemTime::now());
        }
    }
}

impl Drop for RefreshLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct RefreshOptions {
    /// Ignore stored offsets and re-read every file from the start.
    pub full: bool,
}

#[derive(Debug, Default, Clone)]
pub struct RefreshStats {
    pub files_seen: usize,
    pub files_read: usize,
    pub bytes_read: u64,
    pub records: usize,
    pub sessions_labeled: usize,
    pub reconcile: reconcile::ReconcileStats,
    pub elapsed: Duration,
}

/// One source file to consider.
struct Source {
    path: PathBuf,
    vendor: Vendor,
    /// Claude only: the file's attribution.
    claude_ctx: Option<claude::FileCtx>,
    /// Claude main transcript (vs subagent).
    main: bool,
    agent_type: Option<String>,
}

fn claude_sources(root: &Path) -> Vec<Source> {
    let mut out = Vec::new();
    let Ok(projects) = std::fs::read_dir(root) else {
        return out;
    };
    for proj in projects.flatten() {
        let Ok(entries) = std::fs::read_dir(proj.path()) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                // <session>/subagents/agent-<id>.jsonl
                let Some(parent_sid) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                    continue;
                };
                let Ok(subs) = std::fs::read_dir(p.join("subagents")) else { continue };
                for s in subs.flatten() {
                    let sp = s.path();
                    if sp.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let stem = sp.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    let id = stem.strip_prefix("agent-").unwrap_or(&stem).to_string();
                    let agent_type = std::fs::read_to_string(sp.with_extension("meta.json"))
                        .ok()
                        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                        .and_then(|v| v.get("agentType").and_then(|a| a.as_str()).map(String::from));
                    out.push(Source {
                        path: sp,
                        vendor: Vendor::Claude,
                        claude_ctx: Some(claude::FileCtx {
                            session_id: parent_sid.clone(),
                            subagent_id: Some(id),
                        }),
                        main: false,
                        agent_type,
                    });
                }
            } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                let sid = p.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                out.push(Source {
                    path: p,
                    vendor: Vendor::Claude,
                    claude_ctx: Some(claude::FileCtx { session_id: sid, subagent_id: None }),
                    main: true,
                    agent_type: None,
                });
            }
        }
    }
    out
}

fn codex_sources(root: &Path, out: &mut Vec<Source>) {
    let Ok(entries) = std::fs::read_dir(root) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            codex_sources(&p, out);
        } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
            out.push(Source { path: p, vendor: Vendor::Codex, claude_ctx: None, main: true, agent_type: None });
        }
    }
}

/// Read complete lines from `offset`; a trailing line without its newline is
/// left for the next refresh. Returns the new offset.
fn read_from(path: &Path, offset: u64, mut each: impl FnMut(&[u8], u64)) -> std::io::Result<u64> {
    let mut f = std::fs::File::open(path)?;
    f.seek(std::io::SeekFrom::Start(offset))?;
    let mut r = std::io::BufReader::with_capacity(1 << 20, f);
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut pos = offset;
    loop {
        buf.clear();
        let n = r.read_until(b'\n', &mut buf)?;
        if n == 0 || buf.last() != Some(&b'\n') {
            break;
        }
        each(&buf[..n - 1], pos);
        pos += n as u64;
    }
    Ok(pos)
}

struct Parsed {
    records: Vec<Record>,
    offset: u64,
    carry: String,
    session_id: Option<String>,
    subagent_id: Option<String>,
}

fn parse_file(src_path: PathBuf, vendor: Vendor, ctx: Option<claude::FileCtx>, offset: u64, carry: String) -> std::io::Result<Parsed> {
    let mut records = Vec::new();
    match vendor {
        Vendor::Claude => {
            let ctx = ctx.expect("claude source has a ctx");
            let mut c: claude::Carry = serde_json::from_str(&carry).unwrap_or_default();
            let new_offset = read_from(&src_path, offset, |line, _| claude::parse_line(line, &ctx, &mut c, &mut records))?;
            Ok(Parsed {
                records: claude::dedupe_batch(records),
                offset: new_offset,
                carry: serde_json::to_string(&c).unwrap_or_default(),
                session_id: Some(ctx.session_id),
                subagent_id: ctx.subagent_id,
            })
        }
        Vendor::Codex => {
            let mut c: codex::Carry = serde_json::from_str(&carry).unwrap_or_default();
            let new_offset = read_from(&src_path, offset, |line, at| codex::parse_line(line, at, &mut c, &mut records))?;
            let sub = (c.thread_id != c.session_id).then(|| c.thread_id.clone()).flatten();
            Ok(Parsed {
                records,
                offset: new_offset,
                carry: serde_json::to_string(&c).unwrap_or_default(),
                session_id: c.session_id.clone(),
                subagent_id: sub,
            })
        }
    }
}

fn mtime_secs(m: &std::fs::Metadata) -> i64 {
    m.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Run one refresh: ingest new transcript bytes, resolve projects, apply
/// Nucleus labels, price, reconcile. Errors if another refresh is running.
pub async fn refresh(workspace_root: &Path, cfg: &UsageConfig, opts: RefreshOptions) -> Result<RefreshStats> {
    let started = std::time::Instant::now();
    let lock = RefreshLock::acquire(workspace_root)?;
    let pool = open(workspace_root).await?;
    let run_id: i64 = sqlx::query_scalar("INSERT INTO refresh_runs (started_at) VALUES (?1) RETURNING id")
        .bind(crate::timestamp::now())
        .fetch_one(&pool)
        .await?;

    let result = refresh_inner(&pool, workspace_root, cfg, opts, &lock).await;
    let (stats, err) = match result {
        Ok(mut s) => {
            s.elapsed = started.elapsed();
            (s, None)
        }
        Err(e) => (RefreshStats::default(), Some(format!("{e:#}"))),
    };
    sqlx::query(
        "UPDATE refresh_runs SET finished_at = ?1, files_seen = ?2, files_read = ?3, bytes_read = ?4,
                                 rows_written = ?5, error = ?6 WHERE id = ?7",
    )
    .bind(crate::timestamp::now())
    .bind(stats.files_seen as i64)
    .bind(stats.files_read as i64)
    .bind(stats.bytes_read as i64)
    .bind(stats.records as i64)
    .bind(&err)
    .bind(run_id)
    .execute(&pool)
    .await?;
    pool.close().await;
    drop(lock);
    match err {
        Some(e) => bail!(e),
        None => Ok(stats),
    }
}

async fn refresh_inner(
    pool: &SqlitePool,
    workspace_root: &Path,
    cfg: &UsageConfig,
    opts: RefreshOptions,
    lock: &RefreshLock,
) -> Result<RefreshStats> {
    let mut stats = RefreshStats::default();
    let tz = crate::claude_session::nucleus_tz();
    let local = store::Local { tz };
    if store::meta_get(pool, "tz").await?.as_deref() != Some(tz.name()) {
        store::relocalize(pool, local).await?;
        store::meta_set(pool, "tz", tz.name()).await?;
    }

    let mut sources = claude_sources(&crate::config::expand_home(&cfg.claude_projects_dir));
    codex_sources(&crate::config::expand_home(&cfg.codex_sessions_dir), &mut sources);
    stats.files_seen = sources.len();

    for src in sources {
        let path_str = src.path.to_string_lossy().into_owned();
        let Ok(meta) = std::fs::metadata(&src.path) else { continue };
        let size = meta.len() as i64;
        let mtime = mtime_secs(&meta);
        let prev = store::load_file_state(pool, &path_str).await?;
        let (offset, carry) = match &prev {
            Some(p) if !opts.full && p.size == size && p.mtime == mtime => continue,
            Some(p) if !opts.full && size >= p.offset => (p.offset as u64, p.carry.clone()),
            _ => (0u64, String::new()),
        };

        let (path, vendor, ctx) = (src.path.clone(), src.vendor, src.claude_ctx.clone());
        let parsed = tokio::task::spawn_blocking(move || parse_file(path, vendor, ctx, offset, carry))
            .await
            .context("parser task")?;
        let parsed = match parsed {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("usage: reading {path_str}: {e}");
                continue;
            }
        };
        stats.files_read += 1;
        stats.bytes_read += parsed.offset.saturating_sub(offset);

        let state = store::FileState {
            path: path_str.clone(),
            vendor: src.vendor,
            session_id: parsed.session_id.clone(),
            subagent_id: parsed.subagent_id.clone(),
            size,
            mtime,
            offset: parsed.offset as i64,
            carry: parsed.carry,
        };
        let ensure = parsed.session_id.as_deref();
        let facts = store::FileFacts {
            transcript_path: (src.main && src.vendor == Vendor::Claude || src.vendor == Vendor::Codex && parsed.subagent_id.is_none())
                .then_some(path_str.as_str()),
            ensure_session: ensure,
            subagent: match (&parsed.subagent_id, ensure) {
                (Some(sub), Some(parent)) => Some((sub.as_str(), parent, src.agent_type.as_deref())),
                _ => None,
            },
        };
        stats.records += store::write_batch(pool, local, &state, &facts, &parsed.records).await?;
        lock.heartbeat();
    }

    resolve_projects(pool, cfg).await?;
    let labels = attribution::collect(workspace_root).await;
    stats.sessions_labeled = attribution::apply(pool, workspace_root, &labels).await?;

    let table = pricing::PriceTable::new(&cfg.prices);
    reconcile::price_rows(pool, &table).await?;
    stats.reconcile = reconcile::reconcile(pool, &table, local).await?;
    store::meta_set(pool, "prices_as_of", pricing::PRICES_AS_OF).await?;
    store::meta_set(pool, "last_refresh", &crate::timestamp::now()).await?;
    if store::meta_get(pool, "first_refresh").await?.is_none() {
        store::meta_set(pool, "first_refresh", &crate::timestamp::now()).await?;
    }
    Ok(stats)
}

/// Map every session's working directory to a project. Sticky for
/// directories that no longer exist (see `project.rs`).
async fn resolve_projects(pool: &SqlitePool, cfg: &UsageConfig) -> Result<()> {
    // Claude sessions without a cwd line (subagent-only, or a main file
    // that never recorded one): recover the cwd from the encoded project
    // directory the transcript lives in.
    let known: Vec<(String,)> = sqlx::query_as("SELECT DISTINCT cwd FROM sessions WHERE cwd IS NOT NULL")
        .fetch_all(pool)
        .await?;
    let encoded: std::collections::HashMap<String, String> =
        known.iter().map(|(c,)| (project::encode_cwd(c), c.clone())).collect();
    let orphans: Vec<(String, String)> = sqlx::query_as(
        "SELECT s.session_id, f.path FROM sessions s JOIN source_files f ON f.session_id = s.session_id
          WHERE s.cwd IS NULL AND s.vendor = 'claude' GROUP BY s.session_id",
    )
    .fetch_all(pool)
    .await?;
    let projects_root = crate::config::expand_home(&cfg.claude_projects_dir);
    for (sid, path) in orphans {
        let dir = Path::new(&path)
            .strip_prefix(&projects_root)
            .ok()
            .and_then(|rel| rel.components().next())
            .map(|c| c.as_os_str().to_string_lossy().into_owned());
        if let Some(cwd) = dir.and_then(|d| encoded.get(&d)) {
            sqlx::query("UPDATE sessions SET cwd = ?1 WHERE session_id = ?2 AND cwd IS NULL")
                .bind(cwd)
                .bind(&sid)
                .execute(pool)
                .await?;
        }
    }

    let stored: Vec<(String, String, String, String)> =
        sqlx::query_as("SELECT cwd, project_root, project_name, method FROM cwd_projects")
            .fetch_all(pool)
            .await?;
    let mut worktree_parents = std::collections::HashMap::new();
    let mut stored_map = std::collections::HashMap::new();
    for (cwd, root, name, method) in stored {
        if method == "worktree" {
            if let Some(parent) = Path::new(&cwd).parent() {
                worktree_parents.insert(parent.to_string_lossy().into_owned(), root.clone());
            }
        }
        stored_map.insert(cwd, (root, name, method));
    }
    let cwds: Vec<(String,)> = sqlx::query_as("SELECT DISTINCT cwd FROM sessions WHERE cwd IS NOT NULL")
        .fetch_all(pool)
        .await?;
    let probe = project::RealFs;
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    // First pass: resolve existing directories (they teach worktree parents).
    let mut resolved: Vec<(String, project::Resolved)> = Vec::new();
    let mut resolver = project::Resolver {
        probe: &probe,
        home,
        markers: &cfg.worktree_markers,
        encoded,
        worktree_parents,
    };
    for (cwd,) in &cwds {
        if Path::new(cwd).exists() {
            let r = resolver.resolve(cwd);
            if r.method == "worktree" {
                if let Some(parent) = Path::new(cwd).parent() {
                    resolver.worktree_parents.insert(parent.to_string_lossy().into_owned(), r.root.clone());
                }
            }
            resolved.push((cwd.clone(), r));
        }
    }
    for (cwd,) in &cwds {
        if Path::new(cwd).exists() {
            continue;
        }
        if let Some((root, name, method)) = stored_map.get(cwd) {
            // Sticky, unless the stored answer was the weakest fallback and
            // a better one is now available.
            if method != "missing" {
                resolved.push((
                    cwd.clone(),
                    project::Resolved { root: root.clone(), name: name.clone(), method: "stored" },
                ));
                continue;
            }
        }
        resolved.push((cwd.clone(), resolver.resolve(cwd)));
    }

    let mut tx = pool.begin().await?;
    for (cwd, r) in resolved {
        if r.method != "stored" {
            sqlx::query(
                "INSERT INTO cwd_projects (cwd, project_root, project_name, method) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(cwd) DO UPDATE SET project_root = excluded.project_root,
                   project_name = excluded.project_name, method = excluded.method",
            )
            .bind(&cwd)
            .bind(&r.root)
            .bind(&r.name)
            .bind(r.method)
            .execute(&mut *tx)
            .await?;
        }
    }
    sqlx::query(
        "UPDATE sessions SET
           project_root = (SELECT project_root FROM cwd_projects c WHERE c.cwd = sessions.cwd),
           project_name = (SELECT project_name FROM cwd_projects c WHERE c.cwd = sessions.cwd)
         WHERE cwd IS NOT NULL",
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests;
