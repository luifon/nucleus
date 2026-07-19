//! distiller — diary distillation, one consolidated daily pass (ADR-016).
//!
//! Was two launchd jobs (hourly `metabolism` + weekly `contemplation`); now a
//! single daily run with no subcommand. Each invocation:
//!   1. metabolism    — extract candidates from the last day's diaries → _pending.md
//!   2. contemplation — judge them (PROMOTE | MERGE | ARCHIVE | DROP) + prune
//!
//! Persona auto-evolution (ADR-004's "SOUL slot") is intentionally NOT here —
//! that's deferred to the future skill-gap learner (ADR-016), which proposes
//! persona edits as reviewable suggestions rather than silent writes to the
//! operator-personal `personas/<slug>.md` files.

use anyhow::{Context, Result};
use chrono::{Duration, Local, NaiveDate};
use nucleus_core::{
    claude_session::Session,
    config::Settings,
    diary, memory,
    session_profile::{ProfileContext, SessionProfile},
};
use serde::Deserialize;
use std::path::{Path, PathBuf};

const AGENT_NAME: &str = "distiller";

#[tokio::main]
async fn main() -> Result<()> {
    nucleus_core::init_tracing();
    let settings = Settings::load().context("loading settings")?;
    let workspace_root = std::env::current_dir()?;
    let diary_root = workspace_root.join(&settings.diary.root);

    let _ = tokio::process::Command::new("tmux")
        .args(["kill-session", "-t", "nucleus-distiller"])
        .output()
        .await;

    // One daily pass: extract fresh candidates, then judge + archive + prune.
    metabolism(&workspace_root, &diary_root, &settings).await?;
    contemplation(&workspace_root, &diary_root, &settings).await?;
    session_index_maintenance(&workspace_root, &settings).await;
    Ok(())
}

/// ADR-023 daily catch-up: refresh the session-search index and prune
/// junk transcripts. Best-effort — index maintenance must never fail the
/// distillation pass. Prune stays dry-run until
/// `[session_search] prune_apply = true` in nucleus.toml; counts go to
/// the diary either way (no silent caps, ADR-020).
async fn session_index_maintenance(workspace_root: &Path, settings: &Settings) {
    use nucleus_core::session_index;
    let result = async {
        let pool = session_index::open(workspace_root).await?;
        let idx = session_index::update_index(&pool, workspace_root).await?;
        let prune = session_index::prune_junk(
            &pool,
            workspace_root,
            settings.session_search.prune_apply,
            settings.session_search.prune_max_age_days,
        )
        .await?;
        anyhow::Ok((idx, prune))
    }
    .await;
    match result {
        Ok((idx, prune)) => {
            let _ = nucleus_core::diary::record_observation(
                workspace_root,
                "distiller",
                "session-index",
                &format!(
                    "session-search index: {} (re)indexed, {} ineligible, {} unchanged; prune{}: {} junk candidate(s), {} deleted",
                    idx.indexed,
                    idx.ineligible,
                    idx.skipped_unchanged,
                    if prune.dry_run { " (dry-run)" } else { "" },
                    prune.candidates,
                    prune.deleted,
                ),
                nucleus_core::diary::Tag::Observation,
            );
        }
        Err(e) => {
            tracing::warn!(err = %format!("{e:#}"), "session-index maintenance failed");
        }
    }
}

fn list_agent_dirs(diary_root: &Path) -> Result<Vec<(String, PathBuf)>> {
    if !diary_root.exists() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for e in std::fs::read_dir(diary_root)? {
        let e = e?;
        if e.file_type()?.is_dir() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') { continue; }
            out.push((name, e.path()));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn read_recent_entries(agent_dir: &Path, since: chrono::DateTime<Local>) -> Result<String> {
    // Concatenate all daily diary files from `since` through today, inclusive.
    let today = Local::now().date_naive();
    let mut out = String::new();
    let mut date = since.date_naive();
    while date <= today {
        let path = agent_dir.join(format!("{}.md", date));
        if path.exists() {
            out.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
            out.push('\n');
        }
        let Some(next) = date.succ_opt() else { break; };
        date = next;
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct Candidate {
    tag: String,
    body: String,
    #[serde(default)]
    confidence: f64,
}

async fn metabolism(workspace_root: &Path, diary_root: &Path, settings: &Settings) -> Result<()> {
    let agents = list_agent_dirs(diary_root)?;
    if agents.is_empty() {
        tracing::info!("metabolism: no agent diaries found");
        return Ok(());
    }
    // One session reused across agents — pays the ~5s spawn cost once
    // instead of per-agent; into_parts() keeps the manual lifecycle while
    // the profile supplies the posture (ADR-020 — this path used to run
    // without the Settings disallowed_tools).
    let (spawn_opts, ask_opts) = SessionProfile::one_shot_utility(&ProfileContext {
        workspace_root,
        claude: &settings.claude,
        tmux_session: "nucleus-distiller",
        agent_label: "distiller",
    })
    .window_name("metabolism")
    .into_parts();
    let mut session = Session::spawn(spawn_opts)
        .await
        .context("spawning claude session for metabolism")?;
    // Daily pass — scan the last day's entries (was hourly).
    let since = Local::now() - Duration::days(1);
    let mut total_staged = 0usize;
    let mut agents_processed = 0usize;

    for (agent, agent_dir) in agents {
        if agent == AGENT_NAME { continue; }  // distiller doesn't extract from itself
        let body = read_recent_entries(&agent_dir, since)?;
        if body.trim().is_empty() {
            continue;
        }
        agents_processed += 1;

        let prompt = format!(r#"Read these recent diary entries from agent "{agent}". Identify candidates worth
promoting to long-term shared memory. A candidate is a stable user fact, a preference,
a piece of feedback, or a recurring observation — not a one-off task summary.

Output a JSON array (no markdown fences, no prose). Each element:
{{"tag": "FACT|FEEDBACK|OBSERVATION|NOTABLE", "body": "<one or two sentences>", "confidence": <0..1>}}

Empty array if nothing worth promoting.

Diary content:
---
{body}
---"#);

        let raw = session.ask(&prompt, ask_opts.clone()).await?;
        let cleaned = strip_code_fence(&raw);
        let candidates: Vec<Candidate> = match serde_json::from_str(&cleaned) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("metabolism: parse failed for {}: {} — raw: {}", agent, e, cleaned);
                continue;
            }
        };

        if candidates.is_empty() {
            tracing::info!("metabolism: agent {} — no candidates", agent);
            continue;
        }

        append_pending(&agent_dir, &candidates)?;
        total_staged += candidates.len();
        tracing::info!("metabolism: agent {} — {} candidates staged", agent, candidates.len());
    }

    let _ = session.close().await;

    let _ = diary::record_observation(
        workspace_root,
        AGENT_NAME,
        "metabolism",
        &format!("{} agents scanned, {} candidates staged", agents_processed, total_staged),
        diary::Tag::Observation,
    );
    Ok(())
}

fn append_pending(agent_dir: &Path, candidates: &[Candidate]) -> Result<()> {
    let pending = agent_dir.join("_pending.md");
    let mut buf = String::new();
    let now = Local::now();
    buf.push_str(&format!("\n## {} — {} candidates\n", now.format("%Y-%m-%d %H:%M"), candidates.len()));
    for c in candidates {
        buf.push_str(&format!("- [{}] (conf {:.2}) {}\n", c.tag, c.confidence, c.body.trim()));
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true).append(true).open(&pending)?;
    f.write_all(buf.as_bytes())?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct Decision {
    op: String,                  // PROMOTE | MERGE | ARCHIVE | DROP
    name: Option<String>,        // memory file slug for PROMOTE/MERGE
    description: Option<String>, // for PROMOTE
    kind: Option<String>,        // user|feedback|project|reference for PROMOTE
    body: Option<String>,        // markdown body for PROMOTE/MERGE/ARCHIVE
    /// PARA destination for ARCHIVE: e.g. "4-Areas/Nucleus", "0-Inbox",
    /// "5-Resources/Rust-async", "6-Slipbox". Validated against the
    /// vault's allowed top-level buckets in [`apply_decision`]; new
    /// sub-folders under 3-Projects / 4-Areas / 5-Resources are NOT
    /// auto-created (those represent durable user commitments, see
    /// CLAUDE.md Rule 9).
    bucket: Option<String>,
    /// ARCHIVE filename within `bucket`. If None we generate
    /// `YYYY-Www-<agent>.md`. Should be a leaf filename (no path
    /// separators).
    filename: Option<String>,
    reason: Option<String>,      // for log
}

async fn contemplation(workspace_root: &Path, diary_root: &Path, settings: &Settings) -> Result<()> {
    let agents = list_agent_dirs(diary_root)?;
    if agents.is_empty() {
        tracing::info!("contemplation: no agent diaries found");
        return Ok(());
    }
    let vault_path = expand_home(&settings.obsidian.vault_path);
    let (spawn_opts, ask_opts) = SessionProfile::one_shot_utility(&ProfileContext {
        workspace_root,
        claude: &settings.claude,
        tmux_session: "nucleus-distiller",
        agent_label: "distiller",
    })
    .add_dirs(vec![vault_path.clone()])
    .window_name("contemplation")
    .into_parts();
    let mut session = Session::spawn(spawn_opts)
        .await
        .context("spawning claude session for contemplation")?;
    let week_ago = Local::now() - Duration::days(settings.diary.retain_days as i64);
    let vault_summary = summarize_vault(&vault_path);

    for (agent, agent_dir) in &agents {
        if agent == AGENT_NAME { continue; }
        let body = read_recent_entries(agent_dir, week_ago)?;
        let pending = std::fs::read_to_string(agent_dir.join("_pending.md")).unwrap_or_default();
        if body.trim().is_empty() && pending.trim().is_empty() {
            continue;
        }

        let prompt = format!(r#"You are the weekly distiller for agent "{agent}". Read the candidate
observations below and decide an op for each. The vault at {vault:?} is mounted
via --add-dir — read files freely when classifying.

Operations:
  PROMOTE — write a NEW file to Tier 2 (shared, auto-loaded into every claude
            session). Use for short, recurring, behaviorally-binding facts the
            bots need every spawn ("user prefers terse replies", "timezone
            <region/city>", "Discord home channel = X"). One fact per file.
  MERGE   — append/update an EXISTING Tier 2 file. Same rules as PROMOTE for
            kind/name; the body should describe what to add.
  ARCHIVE — write a longer-form note to T3 (the user's PARA-organized Obsidian
            second brain). Use for narrative/decisions/notes the user might
            want to browse later, not facts the bot needs every spawn.
  DROP    — no action; explain why in `reason`.

T2 vs T3 split: the test is "does the bot need this in every spawn?" If yes,
PROMOTE/MERGE. If "user might want to browse this later," ARCHIVE.

ARCHIVE rules (CLAUDE.md Rule 9 — read it if you haven't):
  1. `bucket` MUST be one of:
       "0-Inbox"
       "1-Main-Notes"          (only if capture is explicitly hub/MOC)
       "2-Daily-Notes"         (only for time-anchored entries; name YYYY-MM-DD.md)
       "3-Projects/<existing>"
       "4-Areas/<existing>"
       "5-Resources/<existing>"
       "6-Slipbox"             (atomic evergreen ideas; flat, no sub-folders)
       "7-Archives/<...>"
     DO NOT invent new sub-folders under 3-Projects, 4-Areas, or
     5-Resources — those are the user's durable commitments and need
     human authorship. If you can't find a matching existing sub-folder,
     prefer "6-Slipbox" for atomic ideas or "0-Inbox" for unclassified.
  2. Read the per-bucket README.md to understand what belongs where.
  3. Read the immediate sibling notes in your chosen bucket and add
     [[wiki-links]] to thematically related ones in the body. Don't
     fabricate links to notes that don't exist.
  4. Body must start with YAML frontmatter:
     ---
     created: YYYY-MM-DD
     source: distiller-contemplation
     tags: [free-form-list-or-omit]
     ---
  5. `filename` is a leaf filename like "2026-W19-discord-routing.md"; the
     distiller writes it under `bucket/`. If None, defaults to
     `YYYY-Www-<agent>.md`.

Vault structure right now (so you can pick a real `bucket` and link real siblings):
{vault_summary}

Output a JSON array (no fences, no prose). Each element:
{{
  "op": "PROMOTE|MERGE|ARCHIVE|DROP",
  "name": "kebab-case-slug",                      // PROMOTE/MERGE
  "kind": "user|feedback|project|reference",      // PROMOTE
  "description": "one-line summary",              // PROMOTE
  "bucket": "4-Areas/Nucleus",                    // ARCHIVE
  "filename": "2026-W19-something.md",            // ARCHIVE (optional)
  "body": "markdown body with frontmatter",       // PROMOTE/MERGE/ARCHIVE
  "reason": "why this op"                         // always
}}

DIARY (last {retain_days} days):
---
{body}
---

PENDING CANDIDATES:
---
{pending}
---"#,
            vault = vault_path,
            vault_summary = vault_summary,
            retain_days = settings.diary.retain_days,
            body = body,
            pending = pending,
        );

        let raw = session.ask(&prompt, ask_opts.clone()).await?;
        let cleaned = strip_code_fence(&raw);
        let decisions: Vec<Decision> = match serde_json::from_str(&cleaned) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("contemplation: parse failed for {}: {} — raw: {}", agent, e, cleaned);
                continue;
            }
        };

        let mut counts = std::collections::HashMap::new();
        for d in &decisions {
            *counts.entry(d.op.clone()).or_insert(0) += 1;
            if let Err(e) = apply_decision(agent, d, &vault_path).await {
                tracing::warn!("contemplation: apply failed for {} {:?}: {}", agent, d.op, e);
            }
        }
        tracing::info!("contemplation: agent {} → {:?}", agent, counts);

        // Prune diary files older than retain_days.
        prune_old_diaries(agent_dir, week_ago.date_naive())?;
        // Reset _pending.md.
        let _ = std::fs::write(agent_dir.join("_pending.md"), "");
    }

    let _ = session.close().await;

    let _ = diary::record_observation(
        workspace_root,
        AGENT_NAME,
        "contemplation",
        &format!("processed {} agents", agents.len()),
        diary::Tag::Observation,
    );
    Ok(())
}

fn prune_old_diaries(agent_dir: &Path, before: NaiveDate) -> Result<()> {
    for e in std::fs::read_dir(agent_dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('_') || !name.ends_with(".md") { continue; }
        let stem = name.trim_end_matches(".md");
        if let Ok(d) = stem.parse::<NaiveDate>() {
            if d < before {
                let _ = std::fs::remove_file(e.path());
                tracing::info!("pruned old diary {:?}", e.path());
            }
        }
    }
    Ok(())
}

async fn apply_decision(agent: &str, d: &Decision, vault_path: &Path) -> Result<()> {
    match d.op.as_str() {
        "PROMOTE" | "MERGE" => {
            let name = d.name.clone().context("name required for PROMOTE/MERGE")?;
            let description = d.description.clone().unwrap_or_else(|| format!("from {}", agent));
            let body = d.body.clone().context("body required for PROMOTE/MERGE")?;
            let kind = match d.kind.as_deref().unwrap_or("reference") {
                "user" => memory::Kind::User,
                "feedback" => memory::Kind::Feedback,
                "project" => memory::Kind::Project,
                _ => memory::Kind::Reference,
            };
            let mem = memory::Memory { name: name.clone(), description, kind, body };
            let path = memory::promote(&mem)?;
            tracing::info!("{}: {} {} -> {:?}", agent, d.op, name, path);
        }
        "ARCHIVE" => {
            let body = d.body.clone().context("body required for ARCHIVE")?;
            let path = archive_to_para(agent, vault_path, d.bucket.as_deref(), d.filename.as_deref(), &body)?;
            tracing::info!("{}: ARCHIVE {} chars -> {:?}", agent, body.len(), path);
        }
        "DROP" => {
            tracing::info!("{}: DROP — {}", agent, d.reason.clone().unwrap_or_default());
        }
        other => anyhow::bail!("unknown op: {}", other),
    }
    Ok(())
}

/// Write an ARCHIVE'd note into the user's PARA-organized vault under the
/// requested bucket. Falls back to `0-Inbox/` if the bucket is missing,
/// invalid, points outside the vault, or names a Project/Area/Resource
/// sub-folder that doesn't already exist (per CLAUDE.md Rule 9 — bots
/// don't auto-create those).
fn archive_to_para(
    agent: &str,
    vault_path: &Path,
    bucket: Option<&str>,
    filename: Option<&str>,
    body: &str,
) -> Result<PathBuf> {
    let resolved = resolve_bucket(vault_path, bucket).unwrap_or_else(|reason| {
        tracing::warn!(
            "ARCHIVE: bucket {:?} rejected ({}); falling back to 0-Inbox",
            bucket, reason
        );
        vault_path.join("0-Inbox")
    });
    std::fs::create_dir_all(&resolved)?;

    let leaf = filename
        .map(sanitize_filename)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let week = Local::now().format("%G-W%V");
            format!("{}-{}.md", week, agent)
        });
    let path = resolved.join(leaf);

    use std::io::Write;
    let exists = path.exists();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    if exists {
        writeln!(f, "\n---\n")?;  // separator between appended sessions
    }
    f.write_all(body.trim().as_bytes())?;
    writeln!(f)?;
    Ok(path)
}

/// Resolve a Claude-supplied bucket string into an absolute path inside
/// the vault, OR return `Err(reason)` so the caller can fall back to
/// 0-Inbox.
///
/// Validation:
/// - Must start with one of the canonical top-level dirs (8 of them)
/// - Must not contain `..` or absolute path components
/// - For 3-Projects / 4-Areas / 5-Resources, the named sub-folder MUST
///   already exist (we don't auto-create durable user commitments).
///   0-Inbox, 1-Main-Notes, 2-Daily-Notes, 6-Slipbox, and 7-Archives/...
///   are allowed to create freely.
fn resolve_bucket(vault_path: &Path, bucket: Option<&str>) -> std::result::Result<PathBuf, String> {
    let raw = bucket.ok_or("no bucket supplied")?.trim().trim_matches('/');
    if raw.is_empty() {
        return Err("empty bucket".into());
    }
    if raw.contains("..") || raw.starts_with('/') {
        return Err(format!("path-escape attempt: {raw}"));
    }
    let top = raw.split('/').next().unwrap_or("");
    let allowed_tops = [
        "0-Inbox",
        "1-Main-Notes",
        "2-Daily-Notes",
        "3-Projects",
        "4-Areas",
        "5-Resources",
        "6-Slipbox",
        "7-Archives",
    ];
    if !allowed_tops.contains(&top) {
        return Err(format!("unknown top-level bucket: {top}"));
    }
    let target = vault_path.join(raw);
    let needs_existing_subdir = matches!(top, "3-Projects" | "4-Areas" | "5-Resources")
        && raw.contains('/');
    if needs_existing_subdir && !target.exists() {
        return Err(format!("sub-folder {raw} doesn't exist (won't auto-create)"));
    }
    Ok(target)
}

/// Strip path separators and a few other shenanigans from a Claude-supplied
/// filename. Doesn't enforce extension — Claude can pick `.md` or whatever.
fn sanitize_filename(name: impl AsRef<str>) -> String {
    name.as_ref()
        .chars()
        .filter(|c| !matches!(*c, '/' | '\\' | '\0'))
        .collect::<String>()
        .trim()
        .to_string()
}

/// Tilde-expand a config path. Mirrors the helper in `dashboard/src/main.rs`
/// (kept duplicated to avoid a core/ trip for one tiny function).
fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(p)
    }
}

/// Compact tree summary of the vault's top three levels — fed to Claude
/// so it can pick a real `bucket` and know what siblings exist for linking.
/// Caps depth and breadth to keep the prompt reasonable.
fn summarize_vault(vault: &Path) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "{}/", vault.display());
    let mut tops: Vec<_> = std::fs::read_dir(vault).into_iter()
        .flatten().flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| {
            let n = e.file_name();
            !n.to_string_lossy().starts_with('.')
        })
        .collect();
    tops.sort_by_key(|e| e.file_name());
    for top in tops {
        let top_name = top.file_name().to_string_lossy().into_owned();
        let _ = writeln!(out, "  {}/", top_name);
        let mut subs: Vec<_> = std::fs::read_dir(top.path()).into_iter()
            .flatten().flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .collect();
        subs.sort_by_key(|e| e.file_name());
        for sub in subs.iter().take(20) {
            let sub_name = sub.file_name().to_string_lossy().into_owned();
            let note_count = std::fs::read_dir(sub.path()).into_iter()
                .flatten().flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".md"))
                .count();
            let _ = writeln!(out, "    {}/  ({} notes)", sub_name, note_count);
        }
        if subs.len() > 20 {
            let _ = writeln!(out, "    … and {} more", subs.len() - 20);
        }
        // Also list top-level .md files in this bucket (notes filed directly).
        let mut top_notes: Vec<_> = std::fs::read_dir(top.path()).into_iter()
            .flatten().flatten()
            .filter(|e| {
                let n = e.file_name();
                n.to_string_lossy().ends_with(".md") && n.to_string_lossy() != "README.md"
            })
            .collect();
        top_notes.sort_by_key(|e| e.file_name());
        for n in top_notes.iter().take(10) {
            let _ = writeln!(out, "    {}", n.file_name().to_string_lossy());
        }
        if top_notes.len() > 10 {
            let _ = writeln!(out, "    … and {} more notes", top_notes.len() - 10);
        }
    }
    out
}

fn strip_code_fence(s: &str) -> String {
    let t = s.trim();
    let t = t.trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    t.to_string()
}
