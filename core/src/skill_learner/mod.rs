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
//! - `suggestions` — a proposed new skill (`create`) or a change to an
//!   existing personal skill (`patch`, stored as the full SKILL.md that
//!   results; the reviewer writes it as edits, [`skill_text`]), waiting for
//!   the operator ([`suggestions`]).
//! - `suggestion_evidence` — the sessions a suggestion came from, with short
//!   quotes.
//! - `reviewed_sessions` — per session, how far the learner has read it, so
//!   a session that continued later is reviewed only for its new part
//!   ([`ledger`]).
//! - `review_runs` — one row per review session the learner ran: a part of
//!   a session, a candidate promotion, the diary gap pass, the curator.
//! - `candidates`, `candidate_evidence` — candidate workflows: work the
//!   reviews saw repeated, with the session parts and local dates it was
//!   seen on, until code promotes it to a suggestion ([`candidates`]).
//! - `store_meta` — the store's random id ([`store_id`]).
//!
//! Losing this file loses the pending suggestions and makes the next sweep
//! review every session again. It is not rebuilt from anything.

pub mod candidates;
pub mod ledger;
pub mod skill_text;
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
},
// A random id of this store, made once when the database is created (or
// migrated). Suggestion ids restart in a new store; the WhatsApp notice's
// dedup key carries this id so a new store's notices are not taken for an
// old store's.
crate::migrate::Migration {
    version: 3,
    name: "adr038-store-id",
    step: crate::migrate::Step::Sql(
        "CREATE TABLE IF NOT EXISTS store_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
        INSERT OR IGNORE INTO store_meta (key, value) VALUES ('store_id', lower(hex(randomblob(16))))",
    ),
},
// Every model pass files suggestions (no pass writes skills): new origins
// (`candidate`, `gap`, `curator`), new evidence sources (`diary`,
// `library`), the skills a curator consolidation retires, and review-run
// rows for the passes that read no session part. SQLite cannot change a
// CHECK constraint in place, so the three tables are rebuilt.
crate::migrate::Migration {
    version: 4,
    name: "skill-learner-origins-retires-runs",
    step: crate::migrate::Step::Rust(rebuild_v4),
},
// Candidate workflows: repeated work recorded across days until code
// promotes it to a suggestion (see `candidates`).
crate::migrate::Migration {
    version: 5,
    name: "skill-learner-candidates",
    step: crate::migrate::Step::Sql(
        "CREATE TABLE IF NOT EXISTS candidates (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            name             TEXT NOT NULL,
            description      TEXT NOT NULL,
            skill            TEXT,
            status           TEXT NOT NULL DEFAULT 'open'
                             CHECK (status IN ('open','promoted','expired','failed')),
            suggestion_id    INTEGER REFERENCES suggestions(id),
            failures         INTEGER NOT NULL DEFAULT 0,
            detail           TEXT,
            created_at       TEXT NOT NULL,
            updated_at       TEXT NOT NULL,
            last_evidence_at TEXT NOT NULL,
            closed_at        TEXT
        );
        CREATE UNIQUE INDEX IF NOT EXISTS candidates_open_name ON candidates(name) WHERE status = 'open';
        CREATE TABLE IF NOT EXISTS candidate_evidence (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            candidate_id INTEGER NOT NULL REFERENCES candidates(id),
            session_id   TEXT NOT NULL,
            source       TEXT NOT NULL CHECK (source IN ('transcript','index')),
            agent        TEXT,
            from_pos     INTEGER NOT NULL,
            to_pos       INTEGER NOT NULL,
            local_date   TEXT NOT NULL,
            note         TEXT NOT NULL DEFAULT '',
            created_at   TEXT NOT NULL
        );
        CREATE UNIQUE INDEX IF NOT EXISTS candidate_evidence_part
            ON candidate_evidence(candidate_id, session_id, from_pos, to_pos)",
    ),
}];

/// The v4 tables. Each is created under a temporary name, filled from the
/// old table (same ids), and renamed over it.
const V4_TABLES: &[(&str, &str, &str)] = &[
    (
        "suggestions",
        "CREATE TABLE suggestions_v4 (
            id                 INTEGER PRIMARY KEY AUTOINCREMENT,
            kind               TEXT NOT NULL CHECK (kind IN ('create','patch')),
            name               TEXT NOT NULL,
            draft              TEXT NOT NULL,
            rationale          TEXT NOT NULL,
            base_hash          TEXT,
            origin             TEXT NOT NULL
                               CHECK (origin IN ('live','backfill','manual','candidate','gap','curator')),
            status             TEXT NOT NULL DEFAULT 'pending'
                               CHECK (status IN ('pending','accepted','rejected','superseded')),
            reject_reason      TEXT,
            superseded_by      INTEGER REFERENCES suggestions(id),
            overrides_rejected INTEGER REFERENCES suggestions(id),
            why_new            TEXT,
            edited             INTEGER NOT NULL DEFAULT 0,
            created_at         TEXT NOT NULL,
            updated_at         TEXT NOT NULL,
            decided_at         TEXT,
            retires            TEXT NOT NULL DEFAULT '[]'
        )",
        "id, kind, name, draft, rationale, base_hash, origin, status, reject_reason, superseded_by,
         overrides_rejected, why_new, edited, created_at, updated_at, decided_at",
    ),
    (
        "suggestion_evidence",
        "CREATE TABLE suggestion_evidence_v4 (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            suggestion_id INTEGER NOT NULL REFERENCES suggestions(id),
            session_id    TEXT,
            source        TEXT NOT NULL CHECK (source IN ('transcript','index','manual','diary','library')),
            agent         TEXT,
            note          TEXT NOT NULL DEFAULT '',
            quotes        TEXT NOT NULL DEFAULT '[]',
            created_at    TEXT NOT NULL
        )",
        "id, suggestion_id, session_id, source, agent, note, quotes, created_at",
    ),
    (
        "review_runs",
        "CREATE TABLE review_runs_v4 (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            pass              TEXT NOT NULL CHECK (pass IN ('live','backfill','promote','gap','curate')),
            session_id        TEXT,
            source            TEXT CHECK (source IN ('transcript','index')),
            from_pos          INTEGER,
            to_pos            INTEGER,
            review_session_id TEXT,
            outcome           TEXT NOT NULL,
            created           INTEGER NOT NULL DEFAULT 0,
            evidence_added    INTEGER NOT NULL DEFAULT 0,
            detail            TEXT,
            started_at        TEXT NOT NULL,
            finished_at       TEXT,
            candidate_id      INTEGER
        )",
        "id, pass, session_id, source, from_pos, to_pos, review_session_id, outcome, created, evidence_added,
         detail, started_at, finished_at",
    ),
];

/// Migration 4: rebuild the three tables with their new constraints, on one
/// connection with foreign keys off (a table that other rows reference
/// cannot be dropped and recreated with them on), in one transaction that
/// ends with a foreign key check. Idempotent: a store that already has the
/// new `suggestions` table is left alone, so a crash between the commit and
/// the version row only repeats the check.
fn rebuild_v4(pool: &SqlitePool) -> futures::future::BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        let mut conn = pool.acquire().await?;
        sqlx::query("PRAGMA foreign_keys = OFF").execute(&mut *conn).await?;
        let result = rebuild_v4_tx(&mut conn).await;
        if result.is_err() {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
        }
        sqlx::query("PRAGMA foreign_keys = ON").execute(&mut *conn).await?;
        result
    })
}

async fn rebuild_v4_tx(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    use sqlx::Row;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    let current: String =
        sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'suggestions'")
            .fetch_one(&mut *conn)
            .await?;
    if current.contains("'curator'") {
        sqlx::query("COMMIT").execute(&mut *conn).await?;
        return Ok(());
    }
    for (table, create, columns) in V4_TABLES {
        let seq: Option<i64> = sqlx::query_scalar("SELECT seq FROM sqlite_sequence WHERE name = ?1")
            .bind(table)
            .fetch_optional(&mut *conn)
            .await?;
        sqlx::query(create).execute(&mut *conn).await?;
        sqlx::query(&format!("INSERT INTO {table}_v4 ({columns}) SELECT {columns} FROM {table}"))
            .execute(&mut *conn)
            .await?;
        sqlx::query(&format!("DROP TABLE {table}")).execute(&mut *conn).await?;
        sqlx::query(&format!("ALTER TABLE {table}_v4 RENAME TO {table}")).execute(&mut *conn).await?;
        // Ids are never reused: keep the old table's AUTOINCREMENT counter.
        if let Some(seq) = seq {
            let kept = sqlx::query("UPDATE sqlite_sequence SET seq = MAX(seq, ?2) WHERE name = ?1")
                .bind(table)
                .bind(seq)
                .execute(&mut *conn)
                .await?;
            if kept.rows_affected() == 0 {
                sqlx::query("INSERT INTO sqlite_sequence (name, seq) VALUES (?1, ?2)")
                    .bind(table)
                    .bind(seq)
                    .execute(&mut *conn)
                    .await?;
            }
        }
    }
    sqlx::query("CREATE INDEX IF NOT EXISTS suggestions_status_name ON suggestions(status, name)")
        .execute(&mut *conn)
        .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS suggestion_evidence_suggestion ON suggestion_evidence(suggestion_id)")
        .execute(&mut *conn)
        .await?;
    let broken = sqlx::query("PRAGMA foreign_key_check").fetch_all(&mut *conn).await?;
    if let Some(row) = broken.first() {
        let table: String = row.try_get(0).unwrap_or_default();
        anyhow::bail!("{} rows break a foreign key after the rebuild (first in {table})", broken.len());
    }
    sqlx::query("COMMIT").execute(&mut *conn).await?;
    Ok(())
}

pub async fn open(workspace_root: &Path) -> Result<SqlitePool> {
    let pool = crate::db::open(&workspace_root.join(DB_PATH)).await?;
    crate::migrate::migrate(&pool, MIGRATIONS).await?;
    Ok(pool)
}

/// The random id of this store (see migration 3).
pub async fn store_id(pool: &SqlitePool) -> Result<String> {
    Ok(sqlx::query_scalar("SELECT value FROM store_meta WHERE key = 'store_id'").fetch_one(pool).await?)
}

/// A private copy of the store, migrated to the current schema, for a run
/// that must not write the real one (`skill-gap-learner sweep --dry-run`):
/// the real file is read through a read-only connection (`VACUUM INTO`, a
/// consistent copy that includes the WAL) and never migrated. The copy
/// lives in the returned directory and is deleted with it. A missing store
/// gives an empty, new copy.
pub async fn open_snapshot(workspace_root: &Path) -> Result<(tempfile::TempDir, SqlitePool)> {
    let dir = tempfile::tempdir()?;
    let copy = dir.path().join("skill_learner.db");
    if let Some(ro) = open_read_only(workspace_root).await? {
        sqlx::query("VACUUM INTO ?1").bind(copy.to_string_lossy().as_ref()).execute(&ro).await?;
        ro.close().await;
    }
    let pool = crate::db::open(&copy).await?;
    crate::migrate::migrate(&pool, MIGRATIONS).await?;
    Ok((dir, pool))
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
/// suggestion accept (its write and the archive moves of the skills it
/// retires), the skill-gap learner's archive of stale agent-created skills,
/// and the dashboard's skill moves. Without it an accept could check a
/// patch's base hash, another writer change the skill, and the accept then
/// replace that version. An advisory `flock` on [`LIBRARY_LOCK_PATH`],
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
