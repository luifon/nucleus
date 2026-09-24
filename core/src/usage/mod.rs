//! Usage accounting (ADR-034): token and estimated-dollar usage of every
//! Claude Code and Codex session on the machine, per project, per model,
//! and — for Nucleus-spawned sessions — per agent and reminder.
//!
//! Core owns `memory/usage.db` (ADR-020). The single writer is
//! [`refresh`], reached through `nucleus usage refresh` (the dashboard
//! spawns it) and the distiller's daily pass; an advisory file lock
//! serializes runs. The dashboard opens the DB read-only.
//!
//! The store keeps aggregates permanently. Claude Code deletes transcripts
//! after about 30 days, so a refresh at least that often keeps the record
//! complete; the first refresh backfills whatever still exists.
//!
//! Incremental: each source file's read offset and content fingerprint are
//! stored with the records it produced (one transaction), so a refresh
//! reads only appended bytes, and re-reads a file whole when it was
//! rewritten (see `ingest.rs`). Every key is derived from content, so
//! `--full` converges on the same rows.

pub mod attribution;
pub mod claude;
pub mod codex;
pub mod ingest;
pub mod pricing;
pub mod project;
pub mod query;
pub mod reconcile;
pub mod records;
pub mod schema;
pub mod store;

use crate::config::UsageConfig;
use anyhow::{Context, Result, bail};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const DB_PATH: &str = "memory/usage.db";
pub const LOCK_PATH: &str = "memory/usage-refresh.lock";
/// Warnings kept per refresh run (the counts are always complete).
const MAX_WARNINGS: usize = 20;

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

/// Whether a refresh holds the lock right now. Probes the advisory lock
/// without waiting; the probe holds it for the duration of one system call.
pub fn refresh_running(workspace_root: &Path) -> bool {
    let Ok(f) = std::fs::OpenOptions::new().read(true).open(workspace_root.join(LOCK_PATH)) else {
        return false;
    };
    matches!(f.try_lock(), Err(std::fs::TryLockError::WouldBlock))
}

/// The refresh lock: an exclusive advisory lock (`flock`) on
/// `memory/usage-refresh.lock`, held through an open file descriptor for
/// the whole refresh. The kernel releases it when the descriptor closes,
/// including when the process dies, so there is no staleness timer, no
/// heartbeat, and no lock file to delete. The file itself stays.
pub struct RefreshLock {
    _file: std::fs::File,
}

impl RefreshLock {
    /// Take the lock. A status probe ([`refresh_running`]) holds it for a
    /// moment, so a busy lock is retried for up to one second before the
    /// refresh is declared already running.
    pub async fn acquire(workspace_root: &Path) -> Result<Self> {
        let path = workspace_root.join(LOCK_PATH);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        for attempt in 0..10 {
            match file.try_lock() {
                Ok(()) => {
                    use std::io::Write;
                    let _ = file.set_len(0);
                    let _ = writeln!(&file, "{}", std::process::id());
                    return Ok(Self { _file: file });
                }
                Err(std::fs::TryLockError::WouldBlock) if attempt < 9 => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(std::fs::TryLockError::WouldBlock) => break,
                Err(std::fs::TryLockError::Error(e)) => {
                    return Err(e).with_context(|| format!("locking {}", path.display()));
                }
            }
        }
        bail!("a usage refresh is already running ({} is locked)", path.display())
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
    /// Files that could not be read (permissions, I/O errors, or written in
    /// place while being read). Their data is missing from this refresh;
    /// the next one retries them.
    pub files_failed: usize,
    pub bytes_read: u64,
    pub records: usize,
    /// Relevant lines that were not valid JSON, in the bytes read now.
    pub malformed_lines: i64,
    /// Lines over the reader's size limit, skipped, in the bytes read now.
    pub oversized_lines: i64,
    /// The first [`MAX_WARNINGS`] problems, as text.
    pub warnings: Vec<String>,
    pub sessions_labeled: usize,
    pub reconcile: reconcile::ReconcileStats,
    pub elapsed: Duration,
}

impl RefreshStats {
    /// A refresh that finished but left data out.
    pub fn partial(&self) -> bool {
        self.files_failed > 0 || self.malformed_lines > 0 || self.oversized_lines > 0
    }

    fn warn(&mut self, msg: String) {
        tracing::warn!("usage: {msg}");
        if self.warnings.len() < MAX_WARNINGS {
            self.warnings.push(msg);
        }
    }
}

/// Run one refresh: ingest new transcript bytes, resolve projects, apply
/// Nucleus labels, price, reconcile. Errors if another refresh is running.
pub async fn refresh(workspace_root: &Path, cfg: &UsageConfig, opts: RefreshOptions) -> Result<RefreshStats> {
    let started = std::time::Instant::now();
    let lock = RefreshLock::acquire(workspace_root).await?;
    let pool = open(workspace_root).await?;
    let run_id: i64 = sqlx::query_scalar("INSERT INTO refresh_runs (started_at) VALUES (?1) RETURNING id")
        .bind(crate::timestamp::now())
        .fetch_one(&pool)
        .await?;

    let mut stats = RefreshStats::default();
    let result = refresh_inner(&pool, workspace_root, cfg, opts, &mut stats).await;
    stats.elapsed = started.elapsed();
    let err = result.err().map(|e| format!("{e:#}"));
    sqlx::query(
        "UPDATE refresh_runs SET finished_at = ?1, files_seen = ?2, files_read = ?3, files_failed = ?4,
                                 bytes_read = ?5, rows_written = ?6, malformed_lines = ?7,
                                 oversized_lines = ?8, warnings = ?9, error = ?10 WHERE id = ?11",
    )
    .bind(crate::timestamp::now())
    .bind(stats.files_seen as i64)
    .bind(stats.files_read as i64)
    .bind(stats.files_failed as i64)
    .bind(stats.bytes_read as i64)
    .bind(stats.records as i64)
    .bind(stats.malformed_lines)
    .bind(stats.oversized_lines)
    .bind((!stats.warnings.is_empty()).then(|| serde_json::to_string(&stats.warnings).unwrap_or_default()))
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
    stats: &mut RefreshStats,
) -> Result<()> {
    let tz = crate::claude_session::nucleus_tz();
    let local = store::Local { tz };
    if store::meta_get(pool, "tz").await?.as_deref() != Some(tz.name()) {
        store::relocalize(pool, local).await?;
        store::meta_set(pool, "tz", tz.name()).await?;
    }

    let mut sources = ingest::claude_sources(&crate::config::expand_home(&cfg.claude_projects_dir));
    ingest::codex_sources(&crate::config::expand_home(&cfg.codex_sessions_dir), &mut sources);
    sources.sort_by(|a, b| a.path.cmp(&b.path));
    stats.files_seen = sources.len();

    for src in &sources {
        match ingest::ingest_file(pool, local, src, opts.full).await? {
            Ok(r) => {
                if r.read {
                    stats.files_read += 1;
                }
                stats.bytes_read += r.bytes;
                stats.records += r.records;
                stats.malformed_lines += r.malformed;
                stats.oversized_lines += r.oversized;
                if r.malformed > 0 {
                    stats.warn(format!("{}: {} malformed usage line(s) skipped", src.path.display(), r.malformed));
                }
                if r.oversized > 0 {
                    stats.warn(format!(
                        "{}: {} line(s) over {} MiB skipped",
                        src.path.display(),
                        r.oversized,
                        ingest::MAX_LINE_BYTES >> 20
                    ));
                }
            }
            // Deleted between discovery and read (Claude Code's cleanup):
            // nothing to report; its stored data stays.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                stats.files_failed += 1;
                stats.warn(format!("{}: not read: {e}", src.path.display()));
            }
        }
    }
    store::derive_rows(pool).await?;

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
    Ok(())
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
        repos: Vec::new(),
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
    // Known repositories: roots resolved through a `.git` entry or a marker,
    // now or in an earlier refresh. They anchor the deleted-worktree rules.
    let is_repo = |m: &str| matches!(m, "git" | "marker" | "worktree");
    let mut repos: std::collections::BTreeSet<String> = resolved
        .iter()
        .filter(|(_, r)| is_repo(r.method))
        .map(|(_, r)| r.root.clone())
        .collect();
    repos.extend(stored_map.values().filter(|(_, _, m)| is_repo(m)).map(|(root, _, _)| root.clone()));
    resolver.repos = repos.into_iter().collect();
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
