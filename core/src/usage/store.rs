//! Writes into `memory/usage.db`. Each source file is written in ONE
//! transaction: the deletion of its earlier observations (when the file was
//! rewritten or truncated, or on `--full`), its new records, and its new
//! read state. A crash keeps all of it or none of it; the next refresh
//! re-reads exactly what was not committed, and content-derived keys absorb
//! any overlap.
//!
//! Observations are stored per file (`usage_obs`, `cost_runs`,
//! `limit_events`, `rate_snapshots`, all keyed by the file path). The
//! counted response rows in `usage_rows` are derived from the observations
//! by [`derive_rows`], for the keys a refresh touched (`dirty_keys`).

use super::records::*;
use anyhow::Result;
use chrono::{Datelike, TimeZone, Timelike};
use sqlx::{Sqlite, SqlitePool, Transaction};

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

/// Content fingerprint of the part of a file already read: the first
/// `head_len` bytes and the (up to) [`super::ingest::FINGERPRINT_BYTES`]
/// bytes that end at the read offset.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Fingerprint {
    pub head_len: i64,
    pub head_hash: String,
    pub tail_hash: String,
}

/// Persisted read state of one source file.
#[derive(Debug, Clone)]
pub struct FileState {
    pub path: String,
    pub vendor: Vendor,
    pub session_id: Option<String>,
    pub subagent_id: Option<String>,
    /// File identity (device, inode). A different identity at the same path
    /// is a different file.
    pub dev: i64,
    pub ino: i64,
    pub size: i64,
    pub mtime: i64,
    pub offset: i64,
    pub carry: String,
    pub fingerprint: Fingerprint,
    pub first_ts_ms: Option<i64>,
    /// Relevant lines that failed to parse, over the part read so far.
    pub malformed_lines: i64,
    /// Lines longer than the reader's limit, skipped, over the part read.
    pub oversized_lines: i64,
}

#[derive(sqlx::FromRow)]
struct FileRow {
    vendor: String,
    session_id: Option<String>,
    subagent_id: Option<String>,
    dev: i64,
    ino: i64,
    size: i64,
    mtime: i64,
    offset: i64,
    carry: Option<String>,
    head_len: i64,
    head_hash: String,
    tail_hash: String,
    first_ts_ms: Option<i64>,
    malformed_lines: i64,
    oversized_lines: i64,
}

pub async fn load_file_state(pool: &SqlitePool, path: &str) -> Result<Option<FileState>> {
    let row: Option<FileRow> = sqlx::query_as(
        "SELECT vendor, session_id, subagent_id, dev, ino, size, mtime, offset, carry, head_len,
                head_hash, tail_hash, first_ts_ms, malformed_lines, oversized_lines
           FROM source_files WHERE path = ?1",
    )
    .bind(path)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| FileState {
        path: path.to_string(),
        vendor: if r.vendor == "codex" { Vendor::Codex } else { Vendor::Claude },
        session_id: r.session_id,
        subagent_id: r.subagent_id,
        dev: r.dev,
        ino: r.ino,
        size: r.size,
        mtime: r.mtime,
        offset: r.offset,
        carry: r.carry.unwrap_or_default(),
        fingerprint: Fingerprint { head_len: r.head_len, head_hash: r.head_hash, tail_hash: r.tail_hash },
        first_ts_ms: r.first_ts_ms,
        malformed_lines: r.malformed_lines,
        oversized_lines: r.oversized_lines,
    }))
}

/// Extra facts about the file known to the caller, not the parser.
pub struct FileFacts<'a> {
    /// Main Claude transcript: its own path becomes the session's
    /// transcript path.
    pub transcript_path: Option<&'a str>,
    /// Ensure this session row exists even when the file carries no
    /// session record (subagent files, sessions without a cwd line).
    pub ensure_session: Option<&'a str>,
    /// Claude subagent type from `agent-<id>.meta.json`.
    pub subagent: Option<(&'a str, &'a str, Option<&'a str>)>,
}

/// Delete every observation of `path` and mark its response keys for
/// re-derivation. Runs at the start of the transaction that re-reads the
/// file from its first byte.
pub async fn forget_source(tx: &mut Transaction<'_, Sqlite>, path: &str) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO dirty_keys (key) SELECT key FROM usage_obs WHERE path = ?1")
        .bind(path)
        .execute(&mut **tx)
        .await?;
    for table in ["usage_obs", "cost_runs", "limit_events", "rate_snapshots"] {
        sqlx::query(&format!("DELETE FROM {table} WHERE path = ?1"))
            .bind(path)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// Write one batch of a file's records inside the file's transaction.
pub async fn write_records(
    tx: &mut Transaction<'_, Sqlite>,
    local: Local,
    path: &str,
    vendor: Vendor,
    records: &[Record],
) -> Result<usize> {
    let vendor = vendor.as_str();
    for r in records {
        match r {
            Record::Usage(u) => {
                let (day, hour, dow) = local.fields(u.ts_ms);
                // Within one file, the line with the largest output wins
                // (streamed partial lines precede the final one, possibly in
                // an earlier refresh).
                sqlx::query(
                    "INSERT INTO usage_obs
                       (path, key, vendor, session_id, subagent_id, ts_ms, local_day, local_hour,
                        local_dow, model, input, cache_write_5m, cache_write_1h, cache_read,
                        output, reasoning)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)
                     ON CONFLICT(path, key) DO UPDATE SET
                       ts_ms = excluded.ts_ms, local_day = excluded.local_day,
                       local_hour = excluded.local_hour, local_dow = excluded.local_dow,
                       model = excluded.model, input = excluded.input,
                       cache_write_5m = excluded.cache_write_5m,
                       cache_write_1h = excluded.cache_write_1h,
                       cache_read = excluded.cache_read, output = excluded.output,
                       reasoning = excluded.reasoning
                     WHERE excluded.output >= usage_obs.output",
                )
                .bind(path)
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
                .execute(&mut **tx)
                .await?;
                sqlx::query("INSERT OR IGNORE INTO dirty_keys (key) VALUES (?1)")
                    .bind(&u.key)
                    .execute(&mut **tx)
                    .await?;
                if u.subagent_id.is_some() {
                    sqlx::query("INSERT OR IGNORE INTO sessions (session_id, vendor) VALUES (?1, ?2)")
                        .bind(&u.session_id)
                        .bind(u.vendor.as_str())
                        .execute(&mut **tx)
                        .await?;
                }
            }
            Record::CostRun(c) => {
                for m in &c.models {
                    // A later snapshot of the same run carries the larger
                    // running totals and replaces the earlier one.
                    sqlx::query(
                        "INSERT INTO cost_runs
                           (path, session_id, start_ms, model, snapshot_ts_ms, input, output,
                            cache_read, cache_write, web_search, cost_usd)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                         ON CONFLICT(path, start_ms, model) DO UPDATE SET
                           session_id = excluded.session_id,
                           snapshot_ts_ms = excluded.snapshot_ts_ms, input = excluded.input,
                           output = excluded.output, cache_read = excluded.cache_read,
                           cache_write = excluded.cache_write, web_search = excluded.web_search,
                           cost_usd = excluded.cost_usd",
                    )
                    .bind(path)
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
                    .execute(&mut **tx)
                    .await?;
                }
            }
            Record::Limit(e) => {
                let (day, _, _) = local.fields(e.ts_ms);
                sqlx::query(
                    "INSERT OR IGNORE INTO limit_events
                       (path, key, vendor, session_id, ts_ms, local_day, kind, status, limit_type, resets_at, message)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                )
                .bind(path)
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
                .execute(&mut **tx)
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
                .execute(&mut **tx)
                .await?;
            }
            Record::Rate(r) => {
                // One row per file, slot, reset window and percentage. A
                // reading without a reset time gets reset_key -1, so the
                // uniqueness holds for it too (SQLite treats NULLs in a key
                // as distinct). The newest time the reading was seen wins.
                sqlx::query(
                    "INSERT INTO rate_snapshots
                       (path, vendor, slot, window_minutes, resets_at, reset_key, used_percent, ts_ms, plan_type)
                     VALUES (?1, ?2, ?3, ?4, ?5, COALESCE(?5, -1), ?6, ?7, ?8)
                     ON CONFLICT(path, vendor, slot, reset_key, used_percent) DO UPDATE SET
                       plan_type = CASE WHEN excluded.ts_ms > rate_snapshots.ts_ms
                                        THEN excluded.plan_type ELSE rate_snapshots.plan_type END,
                       window_minutes = CASE WHEN excluded.ts_ms > rate_snapshots.ts_ms
                                        THEN excluded.window_minutes ELSE rate_snapshots.window_minutes END,
                       ts_ms = MAX(rate_snapshots.ts_ms, excluded.ts_ms)",
                )
                .bind(path)
                .bind(vendor)
                .bind(&r.slot)
                .bind(r.window_minutes)
                .bind(r.resets_at)
                .bind(r.used_percent)
                .bind(r.ts_ms)
                .bind(&r.plan_type)
                .execute(&mut **tx)
                .await?;
            }
        }
    }
    Ok(records.len())
}

/// Session and subagent rows plus the file's new read state; the last
/// statements of the file's transaction.
pub async fn finish_file(tx: &mut Transaction<'_, Sqlite>, state: &FileState, facts: &FileFacts<'_>) -> Result<()> {
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
        .execute(&mut **tx)
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
        .execute(&mut **tx)
        .await?;
    }
    sqlx::query(
        "INSERT INTO source_files
           (path, vendor, session_id, subagent_id, dev, ino, size, mtime, offset, carry, head_len,
            head_hash, tail_hash, first_ts_ms, malformed_lines, oversized_lines, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
         ON CONFLICT(path) DO UPDATE SET
           vendor = excluded.vendor, session_id = excluded.session_id,
           subagent_id = excluded.subagent_id, dev = excluded.dev, ino = excluded.ino,
           size = excluded.size, mtime = excluded.mtime, offset = excluded.offset,
           carry = excluded.carry, head_len = excluded.head_len, head_hash = excluded.head_hash,
           tail_hash = excluded.tail_hash, first_ts_ms = excluded.first_ts_ms,
           malformed_lines = excluded.malformed_lines, oversized_lines = excluded.oversized_lines,
           updated_at = excluded.updated_at",
    )
    .bind(&state.path)
    .bind(vendor)
    .bind(&state.session_id)
    .bind(&state.subagent_id)
    .bind(state.dev)
    .bind(state.ino)
    .bind(state.size)
    .bind(state.mtime)
    .bind(state.offset)
    .bind(&state.carry)
    .bind(state.fingerprint.head_len)
    .bind(&state.fingerprint.head_hash)
    .bind(&state.fingerprint.tail_hash)
    .bind(state.first_ts_ms)
    .bind(state.malformed_lines)
    .bind(state.oversized_lines)
    .bind(crate::timestamp::now())
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// Re-derive the counted response row of every dirty key from the
/// observations of all files that contain it. The chosen observation is the
/// one with the largest output (the final line of a streamed response);
/// among equal ones, the file whose first line is oldest (the original
/// session, not a later fork or resume that copied the response), then the
/// path. The choice depends only on the stored observations, so an
/// incremental refresh and `--full` choose the same row. A key no file
/// observes any more (its only file was rewritten without it) is removed.
pub async fn derive_rows(pool: &SqlitePool) -> Result<usize> {
    let mut tx = pool.begin().await?;
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dirty_keys").fetch_one(&mut *tx).await?;
    if n == 0 {
        return Ok(0);
    }
    sqlx::query("DELETE FROM usage_rows WHERE kind = 'response' AND key IN (SELECT key FROM dirty_keys)")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO usage_rows
           (key, vendor, kind, session_id, subagent_id, ts_ms, local_day, local_hour, local_dow,
            model, input, cache_write_5m, cache_write_1h, cache_read, output, reasoning, cost_usd,
            source_path)
         SELECT key, vendor, 'response', session_id, subagent_id, ts_ms, local_day, local_hour,
                local_dow, model, input, cache_write_5m, cache_write_1h, cache_read, output,
                reasoning, NULL, path
           FROM (
             SELECT o.*, ROW_NUMBER() OVER (
                      PARTITION BY o.key
                      ORDER BY o.output DESC, COALESCE(f.first_ts_ms, 9223372036854775807) ASC, o.path ASC
                    ) AS rn
               FROM usage_obs o LEFT JOIN source_files f ON f.path = o.path
              WHERE o.key IN (SELECT key FROM dirty_keys)
           )
          WHERE rn = 1",
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM dirty_keys").execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(n as usize)
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
    let rows: Vec<(String, i64)> = sqlx::query_as("SELECT key, ts_ms FROM usage_rows").fetch_all(&mut *tx).await?;
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
    let rows: Vec<(String, String, i64)> =
        sqlx::query_as("SELECT path, key, ts_ms FROM usage_obs").fetch_all(&mut *tx).await?;
    for (path, key, ts) in rows {
        let (d, h, w) = local.fields(ts);
        sqlx::query(
            "UPDATE usage_obs SET local_day = ?1, local_hour = ?2, local_dow = ?3 WHERE path = ?4 AND key = ?5",
        )
        .bind(d)
        .bind(h)
        .bind(w)
        .bind(path)
        .bind(key)
        .execute(&mut *tx)
        .await?;
    }
    let rows: Vec<(String, String, i64)> =
        sqlx::query_as("SELECT path, key, ts_ms FROM limit_events").fetch_all(&mut *tx).await?;
    for (path, key, ts) in rows {
        sqlx::query("UPDATE limit_events SET local_day = ?1 WHERE path = ?2 AND key = ?3")
            .bind(local.fields(ts).0)
            .bind(path)
            .bind(key)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}
