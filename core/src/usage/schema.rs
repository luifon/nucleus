//! `memory/usage.db` schema (ADR-034). One writer: [`super::refresh`], run
//! by `nucleus usage refresh` and the distiller's daily pass, serialized by
//! the refresh lock. Everyone else opens it read-only.
//!
//! Two layers:
//!
//! - **Per-source-file observations** (`usage_obs`, `cost_runs`,
//!   `limit_events`, `rate_snapshots`): what one transcript file says, keyed
//!   by the file's path. When a file is rewritten or truncated, all of its
//!   observations are deleted and re-read in one transaction.
//! - **Counted rows** (`usage_rows`): one row per API response, derived from
//!   the observations of every file that contains the response (a resumed
//!   or forked session repeats responses of another file), plus the derived
//!   residual and adjustment rows of the cost-state reconciliation. Queries
//!   aggregate over `usage_rows` only.

use crate::migrate::{Migration, Step};

pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "adr034-usage",
    step: Step::Sql(
        "CREATE TABLE IF NOT EXISTS source_files (
            path             TEXT PRIMARY KEY,
            vendor           TEXT NOT NULL,
            session_id       TEXT,
            subagent_id      TEXT,
            dev              INTEGER NOT NULL,
            ino              INTEGER NOT NULL,
            size             INTEGER NOT NULL,
            mtime            INTEGER NOT NULL,
            offset           INTEGER NOT NULL,
            carry            TEXT,
            head_len         INTEGER NOT NULL,
            head_hash        TEXT NOT NULL,
            tail_hash        TEXT NOT NULL,
            first_ts_ms      INTEGER,
            malformed_lines  INTEGER NOT NULL DEFAULT 0,
            oversized_lines  INTEGER NOT NULL DEFAULT 0,
            updated_at       TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS sessions (
            session_id      TEXT PRIMARY KEY,
            vendor          TEXT NOT NULL,
            cwd             TEXT,
            project_root    TEXT,
            project_name    TEXT,
            transcript_path TEXT,
            ai_title        TEXT,
            custom_title    TEXT,
            originator      TEXT,
            agent           TEXT,
            label_source    TEXT,
            reminder_id     INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_sessions_project ON sessions(project_root);
        CREATE INDEX IF NOT EXISTS idx_sessions_agent ON sessions(agent);
        CREATE TABLE IF NOT EXISTS subagents (
            subagent_id   TEXT PRIMARY KEY,
            session_id    TEXT NOT NULL,
            vendor        TEXT NOT NULL,
            agent_type    TEXT
        );
        CREATE TABLE IF NOT EXISTS usage_obs (
            path            TEXT NOT NULL,
            key             TEXT NOT NULL,
            vendor          TEXT NOT NULL,
            session_id      TEXT NOT NULL,
            subagent_id     TEXT,
            ts_ms           INTEGER NOT NULL,
            local_day       TEXT NOT NULL,
            local_hour      INTEGER NOT NULL,
            local_dow       INTEGER NOT NULL,
            model           TEXT NOT NULL,
            input           INTEGER NOT NULL,
            cache_write_5m  INTEGER NOT NULL,
            cache_write_1h  INTEGER NOT NULL,
            cache_read      INTEGER NOT NULL,
            output          INTEGER NOT NULL,
            reasoning       INTEGER NOT NULL,
            PRIMARY KEY (path, key)
        ) WITHOUT ROWID;
        CREATE INDEX IF NOT EXISTS idx_obs_key ON usage_obs(key);
        CREATE INDEX IF NOT EXISTS idx_obs_session ON usage_obs(session_id, key);
        CREATE TABLE IF NOT EXISTS dirty_keys (
            key  TEXT PRIMARY KEY
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS usage_rows (
            key             TEXT PRIMARY KEY,
            vendor          TEXT NOT NULL,
            kind            TEXT NOT NULL,
            session_id      TEXT NOT NULL,
            subagent_id     TEXT,
            ts_ms           INTEGER NOT NULL,
            local_day       TEXT NOT NULL,
            local_hour      INTEGER NOT NULL,
            local_dow       INTEGER NOT NULL,
            model           TEXT NOT NULL,
            input           INTEGER NOT NULL,
            cache_write_5m  INTEGER NOT NULL,
            cache_write_1h  INTEGER NOT NULL,
            cache_read      INTEGER NOT NULL,
            output          INTEGER NOT NULL,
            reasoning       INTEGER NOT NULL,
            cost_usd        REAL,
            source_path     TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_usage_day ON usage_rows(local_day);
        CREATE INDEX IF NOT EXISTS idx_usage_session_ts ON usage_rows(session_id, ts_ms);
        CREATE INDEX IF NOT EXISTS idx_usage_model ON usage_rows(model);
        CREATE TABLE IF NOT EXISTS cost_runs (
            path            TEXT NOT NULL,
            session_id      TEXT NOT NULL,
            start_ms        INTEGER NOT NULL,
            model           TEXT NOT NULL,
            snapshot_ts_ms  INTEGER,
            input           INTEGER NOT NULL,
            output          INTEGER NOT NULL,
            cache_read      INTEGER NOT NULL,
            cache_write     INTEGER NOT NULL,
            web_search      INTEGER NOT NULL,
            cost_usd        REAL NOT NULL,
            PRIMARY KEY (path, start_ms, model)
        );
        CREATE TABLE IF NOT EXISTS limit_events (
            path        TEXT NOT NULL,
            key         TEXT NOT NULL,
            vendor      TEXT NOT NULL,
            session_id  TEXT NOT NULL,
            ts_ms       INTEGER NOT NULL,
            local_day   TEXT NOT NULL,
            kind        TEXT NOT NULL,
            status      INTEGER,
            limit_type  TEXT,
            resets_at   INTEGER,
            message     TEXT,
            PRIMARY KEY (path, key)
        );
        CREATE INDEX IF NOT EXISTS idx_limit_ts ON limit_events(ts_ms);
        CREATE INDEX IF NOT EXISTS idx_limit_key ON limit_events(key);
        CREATE TABLE IF NOT EXISTS rate_snapshots (
            path            TEXT NOT NULL,
            vendor          TEXT NOT NULL,
            slot            TEXT NOT NULL,
            window_minutes  INTEGER,
            resets_at       INTEGER,
            reset_key       INTEGER NOT NULL,
            used_percent    REAL NOT NULL,
            ts_ms           INTEGER NOT NULL,
            plan_type       TEXT,
            PRIMARY KEY (path, vendor, slot, reset_key, used_percent)
        );
        CREATE INDEX IF NOT EXISTS idx_rate_slot_ts ON rate_snapshots(vendor, slot, ts_ms);
        CREATE TABLE IF NOT EXISTS cwd_projects (
            cwd           TEXT PRIMARY KEY,
            project_root  TEXT NOT NULL,
            project_name  TEXT NOT NULL,
            method        TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS reminders_meta (
            reminder_id  INTEGER PRIMARY KEY,
            title        TEXT,
            cron         TEXT,
            status       TEXT,
            created_by   TEXT
        );
        CREATE TABLE IF NOT EXISTS prices (
            model                 TEXT PRIMARY KEY,
            matched_key           TEXT,
            basis                 TEXT,
            source_url            TEXT,
            retrieved             TEXT,
            input                 REAL,
            output                REAL,
            cache_read            REAL,
            cache_write_5m        REAL,
            cache_write_1h        REAL,
            cache_write_inferred  INTEGER NOT NULL DEFAULT 0,
            cache_read_inferred   INTEGER NOT NULL DEFAULT 0,
            long_context_above    INTEGER,
            lc_input              REAL,
            lc_output             REAL,
            lc_cache_read         REAL,
            lc_cache_write        REAL
        );
        CREATE TABLE IF NOT EXISTS meta (
            key    TEXT PRIMARY KEY,
            value  TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS refresh_runs (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            started_at       TEXT NOT NULL,
            finished_at      TEXT,
            files_seen       INTEGER NOT NULL DEFAULT 0,
            files_read       INTEGER NOT NULL DEFAULT 0,
            files_failed     INTEGER NOT NULL DEFAULT 0,
            bytes_read       INTEGER NOT NULL DEFAULT 0,
            rows_written     INTEGER NOT NULL DEFAULT 0,
            malformed_lines  INTEGER NOT NULL DEFAULT 0,
            oversized_lines  INTEGER NOT NULL DEFAULT 0,
            warnings         TEXT,
            error            TEXT
        )",
    ),
}];
