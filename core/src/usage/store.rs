//! Writes into `memory/usage.db`. Each source file's new records and its new
//! read offset commit in ONE transaction, so a crash either keeps both or
//! neither: the next refresh re-reads exactly what was not committed, and
//! the idempotent keys absorb any overlap.

use super::records::*;
use anyhow::Result;
use chrono::{Datelike, TimeZone, Timelike};
use sqlx::SqlitePool;

/// Local calendar fields of a timestamp in the operator's timezone
/// (`NUCLEUS_TZ`). Stored per row so the dashboard aggregates in SQL.
#[derive(Debug, Clone, Copy)]
pub struct Local {
    pub tz: chrono_tz::Tz,
}

impl Local {
    /// `(YYYY-MM-DD, hour 0-23, weekday 0=Monday..6=Sunday)`.
    pub fn fields(&self, ts_ms: i64) -> (String, i64, i64) {
        match self.tz.timestamp_millis_opt(ts_ms).single() {
            Some(d) => (
                d.format("%Y-%m-%d").to_string(),
                d.hour() as i64,
                d.weekday().num_days_from_monday() as i64,
            ),
            None => ("1970-01-01".to_string(), 0, 3),
        }
    }
}

/// Persisted read position of one source file.
#[derive(Debug, Clone)]
pub struct FileState {
    pub path: String,
    pub vendor: Vendor,
    pub session_id: Option<String>,
    pub subagent_id: Option<String>,
    pub size: i64,
    pub mtime: i64,
    pub offset: i64,
    pub carry: String,
}

pub async fn load_file_state(pool: &SqlitePool, path: &str) -> Result<Option<FileState>> {
    let row: Option<(String, Option<String>, Option<String>, i64, i64, i64, Option<String>)> = sqlx::query_as(
        "SELECT vendor, session_id, subagent_id, size, mtime, offset, carry FROM source_files WHERE path = ?1",
    )
    .bind(path)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(vendor, session_id, subagent_id, size, mtime, offset, carry)| FileState {
        path: path.to_string(),
        vendor: if vendor == "codex" { Vendor::Codex } else { Vendor::Claude },
        session_id,
        subagent_id,
        size,
        mtime,
        offset,
        carry: carry.unwrap_or_default(),
    }))
}

/// Extra facts about the file known to the caller, not the parser.
pub struct FileFacts<'a> {
    /// Main Claude transcript: its own path becomes the session's
    /// transcript path.
    pub transcript_path: Option<&'a str>,
    /// Ensure this session row exists even when the batch carries no
    /// session record (subagent files, sessions without a cwd line).
    pub ensure_session: Option<&'a str>,
    /// Claude subagent type from `agent-<id>.meta.json`.
    pub subagent: Option<(&'a str, &'a str, Option<&'a str>)>,
}

/// Commit one file's records and its new state atomically. Returns the
/// number of records written.
pub async fn write_batch(
    pool: &SqlitePool,
    local: Local,
    state: &FileState,
    facts: &FileFacts<'_>,
    records: &[Record],
) -> Result<usize> {
    let mut tx = pool.begin().await?;
    let vendor = state.vendor.as_str();

    if let Some(sid) = facts.ensure_session {
        sqlx::query(
            "INSERT INTO sessions (session_id, vendor, transcript_path) VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET
               transcript_path = COALESCE(excluded.transcript_path, sessions.transcript_path)",
        )
        .bind(sid)
        .bind(vendor)
        .bind(facts.transcript_path)
        .execute(&mut *tx)
        .await?;
    }
    if let Some((sub, parent, agent_type)) = facts.subagent {
        sqlx::query(
            "INSERT INTO subagents (subagent_id, session_id, vendor, agent_type) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(subagent_id) DO UPDATE SET
               agent_type = COALESCE(excluded.agent_type, subagents.agent_type)",
        )
        .bind(sub)
        .bind(parent)
        .bind(vendor)
        .bind(agent_type)
        .execute(&mut *tx)
        .await?;
    }

    for r in records {
        match r {
            Record::Usage(u) => {
                let (day, hour, dow) = local.fields(u.ts_ms);
                sqlx::query(
                    "INSERT INTO usage_rows
                       (key, vendor, kind, session_id, subagent_id, ts_ms, local_day, local_hour,
                        local_dow, model, input, cache_write_5m, cache_write_1h, cache_read,
                        output, reasoning, cost_usd)
                     VALUES (?1, ?2, 'response', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, NULL)
                     ON CONFLICT(key) DO UPDATE SET
                       ts_ms = excluded.ts_ms, local_day = excluded.local_day,
                       local_hour = excluded.local_hour, local_dow = excluded.local_dow,
                       model = excluded.model, input = excluded.input,
                       cache_write_5m = excluded.cache_write_5m,
                       cache_write_1h = excluded.cache_write_1h,
                       cache_read = excluded.cache_read, output = excluded.output,
                       reasoning = excluded.reasoning, cost_usd = NULL
                     WHERE excluded.output >= usage_rows.output",
                )
                .bind(&u.key)
                .bind(u.vendor.as_str())
                .bind(&u.session_id)
                .bind(&u.subagent_id)
                .bind(u.ts_ms)
                .bind(&day)
                .bind(hour)
                .bind(dow)
                .bind(&u.model)
                .bind(u.tokens.input)
                .bind(u.tokens.cache_write_5m)
                .bind(u.tokens.cache_write_1h)
                .bind(u.tokens.cache_read)
                .bind(u.tokens.output)
                .bind(u.reasoning)
                .execute(&mut *tx)
                .await?;
                // Every session whose files contain the response, not only
                // the row's owner: a resumed or forked session repeats
                // responses another session file already recorded, and the
                // reconciliation must see them as observed (reconcile.rs).
                sqlx::query("INSERT OR IGNORE INTO usage_keys (session_id, key) VALUES (?1, ?2)")
                    .bind(&u.session_id)
                    .bind(&u.key)
                    .execute(&mut *tx)
                    .await?;
                if u.subagent_id.is_some() {
                    sqlx::query(
                        "INSERT OR IGNORE INTO sessions (session_id, vendor) VALUES (?1, ?2)",
                    )
                    .bind(&u.session_id)
                    .bind(u.vendor.as_str())
                    .execute(&mut *tx)
                    .await?;
                }
            }
            Record::CostRun(c) => {
                for m in &c.models {
                    sqlx::query(
                        "INSERT INTO cost_runs
                           (session_id, start_ms, model, snapshot_ts_ms, input, output,
                            cache_read, cache_write, web_search, cost_usd)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                         ON CONFLICT(session_id, start_ms, model) DO UPDATE SET
                           snapshot_ts_ms = excluded.snapshot_ts_ms, input = excluded.input,
                           output = excluded.output, cache_read = excluded.cache_read,
                           cache_write = excluded.cache_write, web_search = excluded.web_search,
                           cost_usd = excluded.cost_usd",
                    )
                    .bind(&c.session_id)
                    .bind(c.start_ms)
                    .bind(&m.model)
                    .bind(c.snapshot_ts_ms)
                    .bind(m.input)
                    .bind(m.output)
                    .bind(m.cache_read)
                    .bind(m.cache_write)
                    .bind(m.web_search_requests)
                    .bind(m.cost_usd)
                    .execute(&mut *tx)
                    .await?;
                }
            }
            Record::Limit(e) => {
                let (day, _, _) = local.fields(e.ts_ms);
                sqlx::query(
                    "INSERT OR IGNORE INTO limit_events
                       (key, vendor, session_id, ts_ms, local_day, kind, status, limit_type, resets_at, message)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                )
                .bind(&e.key)
                .bind(e.vendor.as_str())
                .bind(&e.session_id)
                .bind(e.ts_ms)
                .bind(&day)
                .bind(&e.kind)
                .bind(e.status)
                .bind(&e.limit_type)
                .bind(e.resets_at)
                .bind(&e.message)
                .execute(&mut *tx)
                .await?;
            }
            Record::Session(s) => {
                sqlx::query(
                    "INSERT INTO sessions (session_id, vendor, cwd, ai_title, custom_title, originator)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(session_id) DO UPDATE SET
                       cwd = COALESCE(sessions.cwd, excluded.cwd),
                       ai_title = COALESCE(excluded.ai_title, sessions.ai_title),
                       custom_title = COALESCE(excluded.custom_title, sessions.custom_title),
                       originator = COALESCE(sessions.originator, excluded.originator)",
                )
                .bind(&s.session_id)
                .bind(vendor)
                .bind(&s.cwd)
                .bind(&s.ai_title)
                .bind(&s.custom_title)
                .bind(&s.originator)
                .execute(&mut *tx)
                .await?;
            }
            Record::Rate(r) => {
                sqlx::query(
                    "INSERT OR IGNORE INTO rate_snapshots
                       (vendor, slot, window_minutes, resets_at, used_percent, ts_ms, plan_type)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .bind(vendor)
                .bind(&r.slot)
                .bind(r.window_minutes)
                .bind(r.resets_at)
                .bind(r.used_percent)
                .bind(r.ts_ms)
                .bind(&r.plan_type)
                .execute(&mut *tx)
                .await?;
            }
        }
    }

    sqlx::query(
        "INSERT INTO source_files
           (path, vendor, session_id, subagent_id, size, mtime, offset, carry, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(path) DO UPDATE SET
           session_id = excluded.session_id, subagent_id = excluded.subagent_id,
           size = excluded.size, mtime = excluded.mtime, offset = excluded.offset,
           carry = excluded.carry, updated_at = excluded.updated_at",
    )
    .bind(&state.path)
    .bind(vendor)
    .bind(&state.session_id)
    .bind(&state.subagent_id)
    .bind(state.size)
    .bind(state.mtime)
    .bind(state.offset)
    .bind(&state.carry)
    .bind(crate::timestamp::now())
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(records.len())
}

pub async fn meta_get(pool: &SqlitePool, key: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT value FROM meta WHERE key = ?1")
        .bind(key)
        .fetch_optional(pool)
        .await?)
}

pub async fn meta_set(pool: &SqlitePool, key: &str, value: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}

/// Recompute the stored local calendar fields after a timezone change.
pub async fn relocalize(pool: &SqlitePool, local: Local) -> Result<()> {
    let mut tx = pool.begin().await?;
    let rows: Vec<(String, i64)> = sqlx::query_as("SELECT key, ts_ms FROM usage_rows")
        .fetch_all(&mut *tx)
        .await?;
    for (key, ts) in rows {
        let (d, h, w) = local.fields(ts);
        sqlx::query("UPDATE usage_rows SET local_day = ?1, local_hour = ?2, local_dow = ?3 WHERE key = ?4")
            .bind(d)
            .bind(h)
            .bind(w)
            .bind(key)
            .execute(&mut *tx)
            .await?;
    }
    let rows: Vec<(String, i64)> = sqlx::query_as("SELECT key, ts_ms FROM limit_events")
        .fetch_all(&mut *tx)
        .await?;
    for (key, ts) in rows {
        sqlx::query("UPDATE limit_events SET local_day = ?1 WHERE key = ?2")
            .bind(local.fields(ts).0)
            .bind(key)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
