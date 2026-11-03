//! Session search (ADR-023): FTS5 retrieval over session transcripts —
//! the missing layer between T1 (raw transcripts) and T2 (promoted facts).
//!
//! Core owns `memory/session_index.db` (ADR-020 DB-ownership). The index is
//! derived from the transcripts while they exist, but it is NOT rebuildable:
//! Claude Code deletes transcripts after its cleanup period, and the index
//! keeps the turns of every eligible session whose transcript is gone. Those
//! rows are the only copy of that text (the skill-gap learner's backfill
//! reads them, ADR-038). So no code path removes the turns of a session
//! because its transcript file is missing, and the index is never rebuilt
//! from scratch: a full rescan would drop them. Back it up like source data.
//! Three operations:
//!
//! - [`update_index`] — incremental: (re)index transcripts whose
//!   mtime/size changed. Cheap enough to run before every query. It only
//!   visits files that exist; a row whose file is gone is left as it is.
//! - [`search`] — FTS5 match over turn text, newest-session-first bias
//!   via rank, optional agent + age filters.
//! - [`prune_junk`] — delete transcripts that never became eligible
//!   (no substantive exchange) once they are old enough. Dry-run by
//!   default; the `[session_search] prune_apply` toml flag arms it. It
//!   never touches an eligible session, whether or not its file exists.
//!
//! Eligibility (the junk gate): a transcript enters the index only with
//! ≥1 real user turn AND ≥1 assistant text turn after the standard
//! turn filtering (tool noise, system injections, date preambles — the
//! same `last_n_turns` filter the priming machinery uses), and only when
//! its agent label isn't a known synthetic (chat-title spawns etc.).

use crate::claude_session::{TranscriptEventKind, Turn, TurnRole, transcript_events};
use anyhow::{Context, Result};
use sqlx::{Row, SqlitePool};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const DB_PATH: &str = "memory/session_index.db";

/// Agent labels whose sessions are synthetic by construction — never
/// indexed, pruned when old. Matched as prefixes against the runs.jsonl
/// `agent` field (e.g. "chat-title", "healthcheck-probe").
pub const SYNTHETIC_AGENT_PREFIXES: &[&str] = &["chat-title", "healthcheck", "sstest", "test-skill"];

/// True when `agent` is one of the [`SYNTHETIC_AGENT_PREFIXES`].
pub fn is_synthetic_agent(agent: &str) -> bool {
    SYNTHETIC_AGENT_PREFIXES.iter().any(|p| agent.starts_with(p))
}

const MIGRATIONS: &[crate::migrate::Migration] = &[crate::migrate::Migration {
    version: 1,
    name: "adr023-session-index",
    step: crate::migrate::Step::Sql(
        "CREATE TABLE IF NOT EXISTS indexed_sessions (
            session_id  TEXT PRIMARY KEY,
            path        TEXT NOT NULL,
            agent       TEXT,
            mtime       INTEGER NOT NULL,
            size        INTEGER NOT NULL,
            eligible    INTEGER NOT NULL,
            turn_count  INTEGER NOT NULL,
            indexed_at  TEXT NOT NULL
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS turns_fts USING fts5(
            text,
            session_id UNINDEXED,
            agent UNINDEXED,
            role UNINDEXED,
            seq UNINDEXED,
            session_ts UNINDEXED,
            tokenize = 'porter unicode61'
        )",
    ),
}];

pub async fn open(workspace_root: &Path) -> Result<SqlitePool> {
    let pool = crate::db::open(&workspace_root.join(DB_PATH)).await?;
    crate::migrate::migrate(&pool, MIGRATIONS).await?;
    Ok(pool)
}

/// The index opened read-only, for readers that must not create or change
/// it (the skill-gap learner). `None` when there is no index yet.
pub async fn open_read_only(workspace_root: &Path) -> Result<Option<SqlitePool>> {
    let path = workspace_root.join(DB_PATH);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(crate::db::open_read_only(&path).await?))
}

/// An eligible session of the index.
#[derive(Debug, Clone)]
pub struct IndexedSession {
    pub session_id: String,
    /// The transcript path recorded when the session was indexed. The file
    /// may no longer exist.
    pub path: String,
    pub agent: Option<String>,
    /// Transcript mtime (unix seconds) when it was last indexed.
    pub mtime: i64,
    pub turn_count: i64,
}

/// Every eligible session in the index, oldest first.
pub async fn eligible_sessions(pool: &SqlitePool) -> Result<Vec<IndexedSession>> {
    let rows: Vec<(String, String, Option<String>, i64, i64)> = sqlx::query_as(
        "SELECT session_id, path, agent, mtime, turn_count FROM indexed_sessions
          WHERE eligible = 1 ORDER BY mtime, session_id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(session_id, path, agent, mtime, turn_count)| IndexedSession {
            session_id,
            path,
            agent: agent.filter(|a| !a.is_empty()),
            mtime,
            turn_count,
        })
        .collect())
}

/// One indexed turn: its position in the session (`seq`, from 0) and text.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexedTurn {
    pub seq: i64,
    pub role: TurnRole,
    pub text: String,
}

/// The indexed turns of `session_id` with `seq >= from_seq`, in order.
pub async fn session_turns(pool: &SqlitePool, session_id: &str, from_seq: i64) -> Result<Vec<IndexedTurn>> {
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT text, role, CAST(seq AS INTEGER) FROM turns_fts
          WHERE session_id = ?1 AND CAST(seq AS INTEGER) >= ?2
          ORDER BY CAST(seq AS INTEGER)",
    )
    .bind(session_id)
    .bind(from_seq)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(text, role, seq)| IndexedTurn {
            seq,
            role: if role == "assistant" { TurnRole::Assistant } else { TurnRole::User },
            text,
        })
        .collect())
}

/// The Claude Code transcript dir for this workspace
/// (`$HOME/.claude/projects/<encoded-cwd>/`).
pub fn transcripts_dir(workspace_root: &Path) -> PathBuf {
    let encoded = crate::claude_session::project_dir_name(workspace_root);
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".claude").join("projects").join(encoded)
}

/// session_id → agent label, from every `memory/logs/<agent>/runs.jsonl`
/// (ADR-016 run-log). Later rows win; unknown sessions stay unlabeled.
pub fn load_agent_map(workspace_root: &Path) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let logs_root = workspace_root.join("memory/logs");
    let Ok(agents) = std::fs::read_dir(&logs_root) else {
        return map;
    };
    for agent_dir in agents.flatten() {
        let runs = agent_dir.path().join("runs.jsonl");
        let Ok(content) = std::fs::read_to_string(&runs) else {
            continue;
        };
        for line in content.lines() {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                continue;
            };
            if let (Some(sid), Some(agent)) = (
                v.get("session_id").and_then(|s| s.as_str()),
                v.get("agent").and_then(|s| s.as_str()),
            ) {
                map.insert(sid.to_string(), agent.to_string());
            }
        }
    }
    map
}

/// The ADR-023 junk gate. Emptiness is a CONTENT property — even a no-op
/// spawn writes ~20 KB of boilerplate, so file size proves nothing.
pub fn is_substantive(turns: &[Turn], agent: Option<&str>) -> bool {
    if agent.is_some_and(is_synthetic_agent) {
        return false;
    }
    let users = turns.iter().filter(|t| t.role == TurnRole::User).count();
    let assistants = turns.iter().filter(|t| t.role == TurnRole::Assistant).count();
    users >= 1 && assistants >= 1
}

#[derive(Debug, Default)]
pub struct UpdateStats {
    pub scanned: usize,
    pub indexed: usize,
    pub skipped_unchanged: usize,
    pub ineligible: usize,
}

/// Incrementally (re)index every transcript in this workspace's project
/// dir. A session whose (mtime, size) matches its indexed row is skipped;
/// changed sessions are re-extracted and their FTS rows replaced.
pub async fn update_index(pool: &SqlitePool, workspace_root: &Path) -> Result<UpdateStats> {
    let mut stats = UpdateStats::default();
    let dir = transcripts_dir(workspace_root);
    let agent_map = load_agent_map(workspace_root);

    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return Ok(stats), // no transcripts yet — empty index
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(session_id) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        let Ok(meta) = entry.metadata() else { continue };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let size = meta.len() as i64;
        stats.scanned += 1;

        let existing: Option<(i64, i64)> = sqlx::query_as(
            "SELECT mtime, size FROM indexed_sessions WHERE session_id = ?1",
        )
        .bind(&session_id)
        .fetch_optional(pool)
        .await?;
        if existing == Some((mtime, size)) {
            stats.skipped_unchanged += 1;
            continue;
        }

        // A transcript that cannot be read now (deleted after the directory
        // listing, unreadable) keeps its indexed turns: they may be the
        // only copy left.
        let turns: Vec<Turn> = match transcript_events(&path, 0) {
            Ok(read) => read
                .events
                .into_iter()
                .filter_map(|e| match e.kind {
                    TranscriptEventKind::Text { role, text, .. } => Some(Turn { role, text }),
                    _ => None,
                })
                .collect(),
            Err(e) => {
                tracing::warn!(session = %session_id, "session index: cannot read the transcript, keeping its rows: {e}");
                continue;
            }
        };
        let agent = agent_map.get(&session_id).cloned();
        let eligible = is_substantive(&turns, agent.as_deref());
        let session_ts = chrono::DateTime::from_timestamp(mtime, 0)
            .map(|d| d.to_rfc3339())
            .unwrap_or_default();

        let mut tx = pool.begin().await?;
        sqlx::query("DELETE FROM turns_fts WHERE session_id = ?1")
            .bind(&session_id)
            .execute(&mut *tx)
            .await?;
        if eligible {
            for (seq, turn) in turns.iter().enumerate() {
                sqlx::query(
                    "INSERT INTO turns_fts (text, session_id, agent, role, seq, session_ts)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .bind(&turn.text)
                .bind(&session_id)
                .bind(agent.as_deref().unwrap_or(""))
                .bind(match turn.role {
                    TurnRole::User => "user",
                    TurnRole::Assistant => "assistant",
                })
                .bind(seq as i64)
                .bind(&session_ts)
                .execute(&mut *tx)
                .await?;
            }
        }
        sqlx::query(
            "INSERT INTO indexed_sessions
                (session_id, path, agent, mtime, size, eligible, turn_count, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(session_id) DO UPDATE SET
                path=excluded.path, agent=excluded.agent, mtime=excluded.mtime,
                size=excluded.size, eligible=excluded.eligible,
                turn_count=excluded.turn_count, indexed_at=excluded.indexed_at",
        )
        .bind(&session_id)
        .bind(path.to_string_lossy().as_ref())
        .bind(agent.as_deref())
        .bind(mtime)
        .bind(size)
        .bind(eligible as i64)
        .bind(turns.len() as i64)
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        if eligible {
            stats.indexed += 1;
        } else {
            stats.ineligible += 1;
        }
    }
    Ok(stats)
}

#[derive(Debug)]
pub struct Hit {
    pub session_id: String,
    pub agent: String,
    pub role: String,
    pub session_ts: String,
    pub snippet: String,
}

/// FTS5 search over indexed turns. `agent` filters by run-log label
/// (prefix match, so "whatsapp" covers whatsapp-dm etc.); `days` bounds
/// session age.
pub async fn search(
    pool: &SqlitePool,
    query: &str,
    agent: Option<&str>,
    days: Option<i64>,
    limit: i64,
) -> Result<Vec<Hit>> {
    let mut sql = String::from(
        "SELECT snippet(turns_fts, 0, '[', ']', ' … ', 16) AS snip,
                session_id, agent, role, session_ts
           FROM turns_fts
          WHERE turns_fts MATCH ?1",
    );
    if agent.is_some() {
        sql.push_str(" AND agent LIKE ?3 || '%'");
    }
    if days.is_some() {
        sql.push_str(" AND session_ts >= ?4");
    }
    sql.push_str(" ORDER BY rank LIMIT ?2");

    let cutoff = days
        .map(|d| (chrono::Utc::now() - chrono::Duration::days(d)).to_rfc3339())
        .unwrap_or_default();
    let rows = sqlx::query(&sql)
        .bind(query)
        .bind(limit)
        .bind(agent.unwrap_or(""))
        .bind(&cutoff)
        .fetch_all(pool)
        .await
        .context("fts query failed (check FTS5 syntax: bare words, OR, \"phrases\")")?;
    Ok(rows
        .into_iter()
        .map(|r| Hit {
            snippet: r.get("snip"),
            session_id: r.get("session_id"),
            agent: r.get("agent"),
            role: r.get("role"),
            session_ts: r.get("session_ts"),
        })
        .collect())
}

#[derive(Debug, Default)]
pub struct PruneStats {
    pub candidates: usize,
    pub deleted: usize,
    pub dry_run: bool,
}

/// Delete ineligible transcripts older than `max_age_days`, IN FULL
/// (transcript file + index row). Eligible sessions are never touched.
/// `apply=false` (the default posture) only reports what would go.
pub async fn prune_junk(
    pool: &SqlitePool,
    workspace_root: &Path,
    apply: bool,
    max_age_days: i64,
) -> Result<PruneStats> {
    let mut stats = PruneStats { dry_run: !apply, ..Default::default() };
    let cutoff = chrono::Utc::now().timestamp() - max_age_days * 86_400;
    let dir = transcripts_dir(workspace_root);

    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT session_id, path FROM indexed_sessions
          WHERE eligible = 0 AND mtime < ?1",
    )
    .bind(cutoff)
    .fetch_all(pool)
    .await?;

    for (session_id, path) in rows {
        let p = PathBuf::from(&path);
        // Safety: only ever delete .jsonl files inside THIS workspace's
        // transcript dir — a moved/aliased row must not reach elsewhere.
        if !p.starts_with(&dir) || p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        stats.candidates += 1;
        if !apply {
            continue;
        }
        if p.exists() {
            std::fs::remove_file(&p).with_context(|| format!("deleting {}", p.display()))?;
        }
        sqlx::query("DELETE FROM indexed_sessions WHERE session_id = ?1")
            .bind(&session_id)
            .execute(pool)
            .await?;
        stats.deleted += 1;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turn(role: TurnRole, text: &str) -> Turn {
        Turn { role, text: text.to_string() }
    }

    #[test]
    fn substantive_needs_both_sides() {
        let both = [turn(TurnRole::User, "hi"), turn(TurnRole::Assistant, "hello")];
        let user_only = [turn(TurnRole::User, "hi")];
        let assistant_only = [turn(TurnRole::Assistant, "boot banner")];
        assert!(is_substantive(&both, None));
        assert!(!is_substantive(&user_only, None));
        assert!(!is_substantive(&assistant_only, None));
        assert!(!is_substantive(&[], None));
    }

    #[test]
    fn synthetic_agents_never_eligible() {
        let both = [turn(TurnRole::User, "hi"), turn(TurnRole::Assistant, "hello")];
        for a in ["chat-title", "healthcheck-probe", "sstest", "test-skill"] {
            assert!(!is_substantive(&both, Some(a)), "agent {a} must be excluded");
        }
        assert!(is_substantive(&both, Some("whatsapp-dm")));
        assert!(is_substantive(&both, Some("chat"))); // NOT chat-title
    }

    /// End-to-end against a temp workspace: index synthetic transcripts,
    /// search, prune. Exercises the real FTS5 schema + gate + safety rails.
    #[tokio::test]
    async fn index_search_prune_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let workspace = tmp.path().join("ws");
        std::fs::create_dir_all(workspace.join("memory")).unwrap();
        // Redirect HOME so transcripts_dir lands inside the tempdir.
        // (Safe: tests in this module run in-process; the var is only read
        // by transcripts_dir at call time.)
        unsafe { std::env::set_var("HOME", tmp.path()) };
        let dir = transcripts_dir(&workspace);
        std::fs::create_dir_all(&dir).unwrap();

        let substantive = r#"{"type":"user","message":{"role":"user","content":"what did we decide about the orçamento adm fee?"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"We left the orçamento admin fee at 10% pending confirmation."}]}}"#;
        std::fs::write(dir.join("aaaa-real.jsonl"), substantive).unwrap();
        let junk = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"boot banner only"}]}}"#;
        std::fs::write(dir.join("bbbb-junk.jsonl"), junk).unwrap();

        let pool = open(&workspace).await.unwrap();
        let stats = update_index(&pool, &workspace).await.unwrap();
        assert_eq!(stats.scanned, 2);
        assert_eq!(stats.indexed, 1);
        assert_eq!(stats.ineligible, 1);

        // unchanged files are skipped on the next pass
        let again = update_index(&pool, &workspace).await.unwrap();
        assert_eq!(again.skipped_unchanged, 2);

        let hits = search(&pool, "orçamento", None, None, 10).await.unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].session_id.contains("aaaa-real"));

        // porter stemming: "deciding" matches "decide"
        let stemmed = search(&pool, "deciding", None, None, 10).await.unwrap();
        assert!(!stemmed.is_empty());

        // prune: dry-run touches nothing even for old junk
        sqlx::query("UPDATE indexed_sessions SET mtime = 0 WHERE eligible = 0")
            .execute(&pool)
            .await
            .unwrap();
        let dry = prune_junk(&pool, &workspace, false, 14).await.unwrap();
        assert_eq!((dry.candidates, dry.deleted), (1, 0));
        assert!(dir.join("bbbb-junk.jsonl").exists());

        // apply: junk deleted in full, eligible transcript untouched
        let real = prune_junk(&pool, &workspace, true, 14).await.unwrap();
        assert_eq!((real.candidates, real.deleted), (1, 1));
        assert!(!dir.join("bbbb-junk.jsonl").exists());
        assert!(dir.join("aaaa-real.jsonl").exists());

        // Claude Code's cleanup deletes the eligible transcript: its turns
        // stay in the index through update_index and prune_junk, even when
        // the row is old enough to be a prune candidate by age.
        std::fs::remove_file(dir.join("aaaa-real.jsonl")).unwrap();
        sqlx::query("UPDATE indexed_sessions SET mtime = 0").execute(&pool).await.unwrap();
        let after = update_index(&pool, &workspace).await.unwrap();
        assert_eq!(after.scanned, 0);
        let pruned = prune_junk(&pool, &workspace, true, 14).await.unwrap();
        assert_eq!((pruned.candidates, pruned.deleted), (0, 0));
        let kept = eligible_sessions(&pool).await.unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].session_id, "aaaa-real");
        let turns = session_turns(&pool, "aaaa-real", 0).await.unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!((turns[0].seq, turns[0].role), (0, TurnRole::User));
        assert!(turns[1].text.contains("10%"));
        assert_eq!(session_turns(&pool, "aaaa-real", 1).await.unwrap().len(), 1);
        assert!(!search(&pool, "orçamento", None, None, 10).await.unwrap().is_empty());
    }
}
