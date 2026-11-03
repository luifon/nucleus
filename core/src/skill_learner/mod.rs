//! Skill suggestions and the transcript-review ledger (ADR-038).
//!
//! Core owns `memory/skill_learner.db` (ADR-020 DB-ownership). Every write
//! goes through this module; its writers are invocations of the one
//! `nucleus` binary: the skill-gap learner's sweep and backfill, the
//! `nucleus skills` operator commands, and the dashboard. Each state change
//! of a suggestion is one `BEGIN IMMEDIATE` transaction whose `WHERE` clause
//! re-checks the status it changes, so two invocations never both decide
//! the same suggestion.
//!
//! Tables:
//!
//! - `suggestions` — a proposed new skill (`create`) or a full replacement
//!   of an existing personal skill (`patch`), waiting for the operator
//!   ([`suggestions`]).
//! - `suggestion_evidence` — the sessions a suggestion came from, with short
//!   quotes.
//! - `reviewed_sessions` — per session, how far the learner has read it, so
//!   a session that continued later is reviewed only for its new part
//!   ([`ledger`]).
//! - `review_runs` — one row per review session the learner ran.
//!
//! Losing this file loses the pending suggestions and makes the next sweep
//! review every session again. It is not rebuilt from anything.

pub mod ledger;
pub mod suggestions;

use anyhow::Result;
use sqlx::SqlitePool;
use std::path::Path;

pub const DB_PATH: &str = "memory/skill_learner.db";

const MIGRATIONS: &[crate::migrate::Migration] = &[crate::migrate::Migration {
    version: 1,
    name: "adr038-skill-suggestions",
    step: crate::migrate::Step::Sql(
        "CREATE TABLE IF NOT EXISTS suggestions (
            id                 INTEGER PRIMARY KEY AUTOINCREMENT,
            kind               TEXT NOT NULL CHECK (kind IN ('create','patch')),
            name               TEXT NOT NULL,
            draft              TEXT NOT NULL,
            rationale          TEXT NOT NULL,
            base_hash          TEXT,
            origin             TEXT NOT NULL CHECK (origin IN ('live','backfill','manual')),
            status             TEXT NOT NULL DEFAULT 'pending'
                               CHECK (status IN ('pending','accepted','rejected','superseded')),
            reject_reason      TEXT,
            superseded_by      INTEGER REFERENCES suggestions(id),
            overrides_rejected INTEGER REFERENCES suggestions(id),
            why_new            TEXT,
            edited             INTEGER NOT NULL DEFAULT 0,
            created_at         TEXT NOT NULL,
            updated_at         TEXT NOT NULL,
            decided_at         TEXT
        );
        CREATE INDEX IF NOT EXISTS suggestions_status_name ON suggestions(status, name);
        CREATE TABLE IF NOT EXISTS suggestion_evidence (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            suggestion_id INTEGER NOT NULL REFERENCES suggestions(id),
            session_id    TEXT,
            source        TEXT NOT NULL CHECK (source IN ('transcript','index','manual')),
            agent         TEXT,
            note          TEXT NOT NULL DEFAULT '',
            quotes        TEXT NOT NULL DEFAULT '[]',
            created_at    TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS suggestion_evidence_suggestion ON suggestion_evidence(suggestion_id);
        CREATE TABLE IF NOT EXISTS reviewed_sessions (
            session_id TEXT PRIMARY KEY,
            source     TEXT NOT NULL CHECK (source IN ('transcript','index')),
            agent      TEXT,
            position   INTEGER NOT NULL,
            file_size  INTEGER,
            outcome    TEXT NOT NULL,
            reviews    INTEGER NOT NULL DEFAULT 0,
            failures   INTEGER NOT NULL DEFAULT 0,
            signals    TEXT,
            updated_at TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS review_runs (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            pass              TEXT NOT NULL CHECK (pass IN ('live','backfill')),
            session_id        TEXT NOT NULL,
            source            TEXT NOT NULL CHECK (source IN ('transcript','index')),
            from_pos          INTEGER NOT NULL,
            to_pos            INTEGER NOT NULL,
            review_session_id TEXT,
            outcome           TEXT NOT NULL,
            created           INTEGER NOT NULL DEFAULT 0,
            evidence_added    INTEGER NOT NULL DEFAULT 0,
            detail            TEXT,
            started_at        TEXT NOT NULL,
            finished_at       TEXT
        )",
    ),
},
// The transcript's first line (hash), so a rewritten file is read again
// from the start; and the number of text turns read, so a transcript
// Claude Code deletes part-way is continued from the session index.
crate::migrate::Migration {
    version: 2,
    name: "adr038-ledger-head-and-text-turns",
    step: crate::migrate::Step::Sql(
        "ALTER TABLE reviewed_sessions ADD COLUMN head_hash TEXT;
        ALTER TABLE reviewed_sessions ADD COLUMN text_turns INTEGER",
    ),
}];

pub async fn open(workspace_root: &Path) -> Result<SqlitePool> {
    let pool = crate::db::open(&workspace_root.join(DB_PATH)).await?;
    crate::migrate::migrate(&pool, MIGRATIONS).await?;
    Ok(pool)
}

/// The store opened read-only, for read paths that must not create it
/// (the dashboard's lists). `None` when it does not exist yet.
pub async fn open_read_only(workspace_root: &Path) -> Result<Option<SqlitePool>> {
    let path = workspace_root.join(DB_PATH);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(crate::db::open_read_only(&path).await?))
}

/// Serializes writes to the personal skill tree between Nucleus programs:
/// suggestion accept, and the skill-gap learner's writing passes (gap,
/// curator, write-mode review). Without it an accept could check a patch's
/// base hash, the curator rewrite the skill, and the accept then replace
/// the curator's version. An advisory `flock` on [`LIBRARY_LOCK_PATH`],
/// released when the value is dropped or the process ends.
pub struct LibraryLock {
    _file: std::fs::File,
}

/// The library write lock file.
pub const LIBRARY_LOCK_PATH: &str = "memory/skill-library.lock";

/// Take the library write lock, waiting up to `wait` for another holder.
pub async fn library_lock(workspace_root: &Path, wait: std::time::Duration) -> Result<LibraryLock> {
    let path = workspace_root.join(LIBRARY_LOCK_PATH);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(&path)?;
    let deadline = std::time::Instant::now() + wait;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(LibraryLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(std::fs::TryLockError::WouldBlock) => anyhow::bail!(
                "the skill library is being written by another Nucleus program ({} is locked); try again in a few minutes",
                path.display()
            ),
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
}

/// Hex SHA-256 of `bytes`: the base hash a patch suggestion is bound to.
pub fn content_hash(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}
