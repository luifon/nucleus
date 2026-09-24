//! `memory/usage.db` schema (ADR-034). One writer: [`super::refresh`], run
//! by `nucleus usage refresh` and the distiller's daily pass, serialized by
//! the refresh lock. Everyone else opens it read-only.

use crate::migrate::{Migration, Step};

pub const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    name: "adr034-usage",
    step: Step::Sql(
        "CREATE TABLE IF NOT EXISTS source_files (
            path          TEXT PRIMARY KEY,
            vendor        TEXT NOT NULL,
            session_id    TEXT,
            subagent_id   TEXT,
            size          INTEGER NOT NULL,
            mtime         INTEGER NOT NULL,
            offset        INTEGER NOT NULL,
            carry         TEXT,
            updated_at    TEXT NOT NULL
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
            cost_usd        REAL
        );
        CREATE INDEX IF NOT EXISTS idx_usage_day ON usage_rows(local_day);
        CREATE INDEX IF NOT EXISTS idx_usage_session_ts ON usage_rows(session_id, ts_ms);
        CREATE INDEX IF NOT EXISTS idx_usage_model ON usage_rows(model);
        CREATE TABLE IF NOT EXISTS usage_keys (
            session_id  TEXT NOT NULL,
            key         TEXT NOT NULL,
            PRIMARY KEY (session_id, key)
        ) WITHOUT ROWID;
        CREATE TABLE IF NOT EXISTS cost_runs (
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
            PRIMARY KEY (session_id, start_ms, model)
        );
        CREATE TABLE IF NOT EXISTS limit_events (
            key         TEXT PRIMARY KEY,
            vendor      TEXT NOT NULL,
            session_id  TEXT NOT NULL,
            ts_ms       INTEGER NOT NULL,
            local_day   TEXT NOT NULL,
            kind        TEXT NOT NULL,
            status      INTEGER,
            limit_type  TEXT,
            resets_at   INTEGER,
            message     TEXT
        );
        CREATE INDEX IF NOT EXISTS idx_limit_ts ON limit_events(ts_ms);
        CREATE TABLE IF NOT EXISTS rate_snapshots (
            vendor          TEXT NOT NULL,
            slot            TEXT NOT NULL,
            window_minutes  INTEGER,
            resets_at       INTEGER,
            used_percent    REAL NOT NULL,
            ts_ms           INTEGER NOT NULL,
            plan_type       TEXT,
            PRIMARY KEY (vendor, slot, resets_at, used_percent)
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
            model           TEXT PRIMARY KEY,
            matched_key     TEXT,
            source          TEXT,
            input           REAL,
            output          REAL,
            cache_read      REAL,
            cache_write_5m  REAL,
            cache_write_1h  REAL
        );
        CREATE TABLE IF NOT EXISTS meta (
            key    TEXT PRIMARY KEY,
            value  TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS refresh_runs (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            started_at      TEXT NOT NULL,
            finished_at     TEXT,
            files_seen      INTEGER NOT NULL DEFAULT 0,
            files_read      INTEGER NOT NULL DEFAULT 0,
            bytes_read      INTEGER NOT NULL DEFAULT 0,
            rows_written    INTEGER NOT NULL DEFAULT 0,
            error           TEXT
        )",
    ),
}];
