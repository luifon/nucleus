//! SQLite store for reminders (ADR-006).
//!
//! Three tables share `memory/reminders.db`:
//!
//! - `reminders` — the canonical record: body, cron expression, lifecycle
//!   status, denormalized `next_fire_at`. One row per scheduled thing.
//! - `reminder_channels` — per-fire delivery state. A reminder fans out
//!   into N channels; each row tracks retries independently. Reset to
//!   `pending` after every successful fire.
//! - `reminder_fires` — append-only audit log. One row per (fire,
//!   channel) attempt, success or failure.
//!
//! Cron expressions are 5-field standard cron, evaluated in `NUCLEUS_TZ`
//! (default `America/Sao_Paulo`). `next_fire_at` and `last_fired_at` are
//! stored as UTC RFC3339.
//!
//! Channel codes (used in `reminder_channels.channel`):
//!   - "discord-home"  → DISCORD_HOME_CHANNEL_ID
//!   - "alfred"        → WhatsApp Alfred group (via outbound_queue in
//!                       memory/whatsapp.db; Alfred drains every 5s)
//!   - "braindump"     → WhatsApp Brain Dump group (same path)

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, TimeZone, Utc};
use chrono_tz::Tz;
use croner::Cron;
use sqlx::{Row, SqlitePool};
use std::path::Path;
use std::str::FromStr;

pub const CHANNEL_DISCORD_HOME: &str = "discord-home";
pub const CHANNEL_ALFRED: &str = "alfred";
pub const CHANNEL_BRAINDUMP: &str = "braindump";
/// Calendar event delivery via JARVIS + Claude.ai Calendar MCP (ADR-007).
pub const CHANNEL_CALENDAR: &str = "calendar";

pub const KNOWN_CHANNELS: &[&str] = &[
    CHANNEL_DISCORD_HOME,
    CHANNEL_ALFRED,
    CHANNEL_BRAINDUMP,
    CHANNEL_CALENDAR,
];

/// Per-channel retry budget before marking the channel `failed` for this fire.
pub const MAX_ATTEMPTS: i64 = 3;

pub fn is_known_channel(c: &str) -> bool {
    KNOWN_CHANNELS.contains(&c)
}

/// Resolve the operator's timezone for cron evaluation. Prefers
/// `NUCLEUS_TZ`, falls back to POSIX `TZ`, then to `America/Sao_Paulo`
/// per ADR-006.
pub fn nucleus_tz() -> Tz {
    let candidates = [std::env::var("NUCLEUS_TZ").ok(), std::env::var("TZ").ok()];
    for c in candidates.iter().flatten() {
        if c.is_empty() {
            continue;
        }
        if let Ok(tz) = c.parse::<Tz>() {
            return tz;
        }
    }
    chrono_tz::America::Sao_Paulo
}

pub async fn open(path: &Path) -> Result<SqlitePool> {
    let pool = nucleus_core::db::open(path).await?;
    ensure_schema(&pool).await?;
    Ok(pool)
}

/// Add ADR-006 columns + tables idempotently. Pre-ADR-006 columns
/// (`due_at`, `fired_at`, `fired_msg_id`, `cancelled_at`) are left in
/// place but no longer written by the new code path; they survive as
/// dead columns so the migration can replay safely on any historical
/// snapshot.
async fn ensure_schema(pool: &SqlitePool) -> Result<()> {
    // Base table — created fresh on new installs with the full ADR-006
    // shape. On existing DBs this is a no-op (table already exists) and
    // we follow up with ALTER TABLE for the new columns.
    // Base shape for fresh installs. On DBs that pre-date ADR-006,
    // this is a no-op (the table already exists with the legacy
    // shape, including the NOT NULL `due_at` column); the ALTERs +
    // backfill + DROP COLUMN sweep below brings them to parity.
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS reminders (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            body            TEXT    NOT NULL,
            cron            TEXT,
            one_shot        INTEGER NOT NULL DEFAULT 0,
            status          TEXT    NOT NULL DEFAULT 'active',
            next_fire_at    TEXT,
            last_fired_at   TEXT,
            paused_until    TEXT,
            created_at      TEXT    NOT NULL,
            created_by      TEXT    NOT NULL DEFAULT 'user',
            system_prompt   TEXT
        );
        "#,
    )
    .execute(pool)
    .await?;

    // Additive ALTERs for DBs that pre-date ADR-006/008. SQLite has no
    // `ADD COLUMN IF NOT EXISTS`, so we tolerate "duplicate column"
    // errors explicitly.
    let alters = [
        "ALTER TABLE reminders ADD COLUMN cron TEXT",
        "ALTER TABLE reminders ADD COLUMN one_shot INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE reminders ADD COLUMN next_fire_at TEXT",
        "ALTER TABLE reminders ADD COLUMN last_fired_at TEXT",
        "ALTER TABLE reminders ADD COLUMN paused_until TEXT",
        "ALTER TABLE reminders ADD COLUMN created_by TEXT NOT NULL DEFAULT 'user'",
        // ADR-008: when set, the reminder spawns a one-shot Claude
        // session at fire time and uses this string as the ask()
        // payload (NOT as a literal --append-system-prompt — the
        // firing path adds a generic reminders persona for that).
        // body XOR system_prompt is enforced in code, not SQL.
        "ALTER TABLE reminders ADD COLUMN system_prompt TEXT",
    ];
    for stmt in alters {
        if let Err(e) = sqlx::query(stmt).execute(pool).await {
            let msg = e.to_string();
            // sqlx wraps the SQLite error; both "duplicate column" and
            // "already exists" surface in different sqlite versions.
            if !msg.contains("duplicate column") && !msg.contains("already exists") {
                return Err(anyhow!("schema migration failed on `{stmt}`: {e}"));
            }
        }
    }

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_reminders_next_fire
              ON reminders(next_fire_at)
              WHERE next_fire_at IS NOT NULL",
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS reminder_channels (
            reminder_id     INTEGER NOT NULL REFERENCES reminders(id) ON DELETE CASCADE,
            channel         TEXT    NOT NULL,
            status          TEXT    NOT NULL DEFAULT 'pending',
            attempts        INTEGER NOT NULL DEFAULT 0,
            last_error      TEXT,
            last_attempt_at TEXT,
            PRIMARY KEY (reminder_id, channel)
        );
        "#,
    )
    .execute(pool)
    .await?;

    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS reminder_fires (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            reminder_id     INTEGER NOT NULL REFERENCES reminders(id) ON DELETE CASCADE,
            fired_at        TEXT    NOT NULL,
            channel         TEXT    NOT NULL,
            success         INTEGER NOT NULL,
            msg_id          TEXT,
            error           TEXT
        );
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_reminder_fires_at ON reminder_fires(fired_at DESC)",
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_reminder_fires_reminder
              ON reminder_fires(reminder_id, fired_at DESC)",
    )
    .execute(pool)
    .await?;

    backfill_pre_adr6(pool).await?;

    // Drop the legacy index that pins `due_at` — without this, the
    // subsequent DROP COLUMN due_at fails (SQLite forbids dropping
    // indexed columns).
    let _ = sqlx::query("DROP INDEX IF EXISTS idx_reminders_due_status")
        .execute(pool)
        .await;

    // Now that any legacy rows have been converted, drop the dead
    // columns. SQLite 3.35+ (macOS Sequoia, all supported hosts)
    // supports DROP COLUMN; ignore "no such column" errors for
    // fresh installs where they were never created.
    let drops = [
        "ALTER TABLE reminders DROP COLUMN due_at",
        "ALTER TABLE reminders DROP COLUMN channel",
        "ALTER TABLE reminders DROP COLUMN fired_at",
        "ALTER TABLE reminders DROP COLUMN fired_msg_id",
        "ALTER TABLE reminders DROP COLUMN cancelled_at",
    ];
    for stmt in drops {
        if let Err(e) = sqlx::query(stmt).execute(pool).await {
            let msg = e.to_string();
            if !msg.contains("no such column") {
                return Err(anyhow!("dropping legacy column failed on `{stmt}`: {e}"));
            }
        }
    }

    Ok(())
}

/// Convert pre-ADR-006 rows (those with `due_at` set but `cron` NULL)
/// into the new shape: synthesize a one-shot cron from `due_at`, copy
/// `due_at` → `next_fire_at`, map the old status, and seed a
/// `reminder_channels` row from the legacy `channel` column.
///
/// Idempotent: only touches rows where `cron IS NULL`.
async fn backfill_pre_adr6(pool: &SqlitePool) -> Result<()> {
    // Backfill only runs when the legacy `due_at` column is still
    // present (pre-ADR-006 DBs). On fresh installs and on DBs already
    // migrated, this is a no-op — preventing the "no such column"
    // failure mode when the second leg of the migration (DROP COLUMN
    // sweep) ran successfully on a previous boot.
    let cols: Vec<String> = sqlx::query("PRAGMA table_info(reminders)")
        .fetch_all(pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| r.get::<String, _>("name"))
        .collect();
    if !cols.iter().any(|n| n == "due_at") {
        return Ok(());
    }

    let rows = sqlx::query(
        "SELECT id, due_at, channel, status, fired_at, cancelled_at
           FROM reminders
          WHERE cron IS NULL",
    )
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(());
    }

    let tz = nucleus_tz();
    for r in rows {
        let id: i64 = r.get("id");
        let due_at: Option<String> = r.try_get("due_at").ok();
        let channel: Option<String> = r.try_get("channel").ok();
        let old_status: String = r.get("status");
        let fired_at: Option<String> = r.try_get("fired_at").ok();
        let cancelled_at: Option<String> = r.try_get("cancelled_at").ok();

        let Some(due_at) = due_at else {
            // No legacy timestamp to convert; mark cancelled so the
            // ticker ignores it. This shouldn't happen in real data.
            sqlx::query("UPDATE reminders SET cron = '0 0 1 1 *', one_shot = 1, status = 'cancelled' WHERE id = ?1")
                .bind(id).execute(pool).await?;
            continue;
        };

        let due_utc = DateTime::parse_from_rfc3339(&due_at)
            .map(|d| d.with_timezone(&Utc))
            .context("legacy due_at parse")?;
        let local = due_utc.with_timezone(&tz);
        // Cron: minute hour day month *  — every year on this date, but
        // the one_shot flag prevents repeat firing.
        let cron = format!(
            "{} {} {} {} *",
            local.format("%M"),
            local.format("%-H"),
            local.format("%-d"),
            local.format("%-m"),
        );

        let (new_status, next_fire_at): (&str, Option<String>) = match old_status.as_str() {
            "pending" => ("pending", Some(due_utc.to_rfc3339())),
            "fired" => ("fired", None),
            "cancelled" => ("cancelled", None),
            _ => ("cancelled", None),
        };

        sqlx::query(
            "UPDATE reminders
                SET cron = ?1,
                    one_shot = 1,
                    status = ?2,
                    next_fire_at = ?3,
                    last_fired_at = COALESCE(?4, last_fired_at),
                    paused_until = COALESCE(paused_until, NULL),
                    created_by = COALESCE(NULLIF(created_by, ''), 'user')
              WHERE id = ?5",
        )
        .bind(&cron)
        .bind(new_status)
        .bind(&next_fire_at)
        .bind(&fired_at)
        .bind(id)
        .execute(pool)
        .await?;

        // Synthesize the channel row. If the row was already
        // `fired`/`cancelled`, mark the channel as `sent`/`failed`
        // accordingly so the next reset logic doesn't try to redeliver
        // ancient history.
        if let Some(ch) = channel {
            let ch_status = match new_status {
                "pending" => "pending",
                "fired" => "sent",
                _ => "failed",
            };
            sqlx::query(
                "INSERT OR IGNORE INTO reminder_channels
                    (reminder_id, channel, status, attempts, last_attempt_at)
                 VALUES (?1, ?2, ?3, 0, ?4)",
            )
            .bind(id)
            .bind(&ch)
            .bind(ch_status)
            .bind(fired_at.clone().or(cancelled_at.clone()))
            .execute(pool)
            .await?;
        }
    }

    tracing::info!("reminders: backfilled pre-ADR-006 rows");
    Ok(())
}

#[derive(Debug, Clone)]
pub struct Reminder {
    pub id: i64,
    pub body: String,
    pub cron: String,
    pub one_shot: bool,
    pub status: String,
    pub next_fire_at: Option<String>,
    pub last_fired_at: Option<String>,
    pub paused_until: Option<String>,
    pub created_at: String,
    pub created_by: String,
    /// ADR-008: when Some, this reminder fires by spawning a one-shot
    /// Claude session and using the stored string as the first ask()
    /// payload (the firing path adds its own generic persona via
    /// --append-system-prompt). XOR with `body`: exactly one is
    /// non-empty, enforced at CLI insert time.
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ChannelRow {
    pub reminder_id: i64,
    pub channel: String,
    pub status: String,
    pub attempts: i64,
    pub last_error: Option<String>,
    pub last_attempt_at: Option<String>,
}

async fn load_reminder(pool: &SqlitePool, id: i64) -> Result<Option<Reminder>> {
    let row = sqlx::query(
        "SELECT id, body, cron, one_shot, status, next_fire_at,
                last_fired_at, paused_until, created_at, created_by,
                system_prompt
           FROM reminders WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(row_to_reminder))
}

fn row_to_reminder(r: sqlx::sqlite::SqliteRow) -> Reminder {
    Reminder {
        id: r.get("id"),
        body: r.get("body"),
        cron: r.try_get::<Option<String>, _>("cron").ok().flatten().unwrap_or_default(),
        one_shot: r.try_get::<i64, _>("one_shot").unwrap_or(0) != 0,
        status: r.get("status"),
        next_fire_at: r.try_get::<Option<String>, _>("next_fire_at").ok().flatten(),
        last_fired_at: r.try_get::<Option<String>, _>("last_fired_at").ok().flatten(),
        paused_until: r.try_get::<Option<String>, _>("paused_until").ok().flatten(),
        created_at: r.get("created_at"),
        created_by: r.try_get::<Option<String>, _>("created_by").ok().flatten().unwrap_or_else(|| "user".into()),
        system_prompt: r.try_get::<Option<String>, _>("system_prompt").ok().flatten(),
    }
}

/// Parse a cron expression and compute the next match strictly after
/// `from`, interpreted in `tz`, returned as UTC.
pub fn next_match_utc(cron: &str, from: DateTime<Utc>, tz: Tz) -> Result<DateTime<Utc>> {
    let parsed = Cron::from_str(cron)
        .map_err(|e| anyhow!("invalid cron {:?}: {e}", cron))?;
    let local = from.with_timezone(&tz);
    let next = parsed
        .find_next_occurrence(&local, false)
        .map_err(|e| anyhow!("cron has no future match: {e}"))?;
    Ok(next.with_timezone(&Utc))
}

/// Compute the cron + next_fire_at for a one-shot `--at <iso>` add.
/// Returns (cron_string, next_fire_at_utc). The cron is uniquely
/// satisfied by `at` itself (minute, hour, day, month) so the one_shot
/// flag is what stops it from re-firing next year.
pub fn one_shot_cron(at_local: DateTime<Tz>) -> (String, DateTime<Utc>) {
    let cron = format!(
        "{} {} {} {} *",
        at_local.format("%M"),
        at_local.format("%-H"),
        at_local.format("%-d"),
        at_local.format("%-m"),
    );
    (cron, at_local.with_timezone(&Utc))
}

/// Insert a reminder + its channel rows in a single transaction.
/// Returns the new reminder id.
pub async fn insert_with_channels(
    pool: &SqlitePool,
    body: &str,
    cron: &str,
    one_shot: bool,
    next_fire_at: DateTime<Utc>,
    channels: &[String],
    created_by: &str,
    system_prompt: Option<&str>,
) -> Result<i64> {
    if channels.is_empty() {
        bail!("at least one channel is required");
    }
    for c in channels {
        if !is_known_channel(c) {
            bail!(
                "unknown channel {:?}; supported: {}",
                c,
                KNOWN_CHANNELS.join(", ")
            );
        }
    }

    let now = Utc::now().to_rfc3339();
    let initial_status = if one_shot { "pending" } else { "active" };
    let mut tx = pool.begin().await?;
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO reminders
            (body, cron, one_shot, status, next_fire_at, created_at, created_by, system_prompt)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         RETURNING id",
    )
    .bind(body)
    .bind(cron)
    .bind(one_shot as i64)
    .bind(initial_status)
    .bind(next_fire_at.to_rfc3339())
    .bind(&now)
    .bind(created_by)
    .bind(system_prompt)
    .fetch_one(&mut *tx)
    .await
    .context("insert reminder")?;
    let id = row.0;

    for c in channels {
        sqlx::query(
            "INSERT INTO reminder_channels (reminder_id, channel, status, attempts)
             VALUES (?1, ?2, 'pending', 0)",
        )
        .bind(id)
        .bind(c)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(id)
}

/// All reminders ordered by next_fire_at, optionally including
/// terminal states.
pub async fn list_all(
    pool: &SqlitePool,
    include_fired: bool,
    include_cancelled: bool,
) -> Result<Vec<Reminder>> {
    let mut q = String::from(
        "SELECT id, body, cron, one_shot, status, next_fire_at,
                last_fired_at, paused_until, created_at, created_by,
                system_prompt
           FROM reminders WHERE 1=1",
    );
    if !include_fired {
        q.push_str(" AND status != 'fired'");
    }
    if !include_cancelled {
        q.push_str(" AND status != 'cancelled'");
    }
    q.push_str(" ORDER BY COALESCE(next_fire_at, '9999') ASC, id ASC");

    let rows = sqlx::query(&q).fetch_all(pool).await?;
    Ok(rows.into_iter().map(row_to_reminder).collect())
}

pub async fn get(pool: &SqlitePool, id: i64) -> Result<Option<Reminder>> {
    load_reminder(pool, id).await
}

pub async fn channels_for(pool: &SqlitePool, reminder_id: i64) -> Result<Vec<ChannelRow>> {
    let rows = sqlx::query(
        "SELECT reminder_id, channel, status, attempts, last_error, last_attempt_at
           FROM reminder_channels WHERE reminder_id = ?1 ORDER BY channel ASC",
    )
    .bind(reminder_id)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| ChannelRow {
            reminder_id: r.get("reminder_id"),
            channel: r.get("channel"),
            status: r.get("status"),
            attempts: r.get("attempts"),
            last_error: r.try_get("last_error").ok(),
            last_attempt_at: r.try_get("last_attempt_at").ok(),
        })
        .collect())
}

/// Reminders that should fire on this tick — active/pending status, a
/// next_fire_at in the past or right now. Returns rows together with
/// the still-pending channel rows (channels already `sent` or `failed`
/// from a previous tick are excluded; firing them again would be the
/// duplicate-delivery bug we explicitly avoid).
pub async fn pending_due_with_channels(
    pool: &SqlitePool,
    now: DateTime<Utc>,
) -> Result<Vec<(Reminder, Vec<ChannelRow>)>> {
    let cutoff = now.to_rfc3339();
    let rems = sqlx::query(
        "SELECT id, body, cron, one_shot, status, next_fire_at,
                last_fired_at, paused_until, created_at, created_by,
                system_prompt
           FROM reminders
          WHERE status IN ('active', 'pending')
            AND next_fire_at IS NOT NULL
            AND next_fire_at <= ?1
          ORDER BY next_fire_at ASC",
    )
    .bind(&cutoff)
    .fetch_all(pool)
    .await?;

    let mut out = Vec::with_capacity(rems.len());
    for r in rems {
        let reminder = row_to_reminder(r);
        let chans = sqlx::query(
            "SELECT reminder_id, channel, status, attempts, last_error, last_attempt_at
               FROM reminder_channels
              WHERE reminder_id = ?1 AND status = 'pending'
              ORDER BY channel ASC",
        )
        .bind(reminder.id)
        .fetch_all(pool)
        .await?;
        let chans: Vec<ChannelRow> = chans
            .into_iter()
            .map(|r| ChannelRow {
                reminder_id: r.get("reminder_id"),
                channel: r.get("channel"),
                status: r.get("status"),
                attempts: r.get("attempts"),
                last_error: r.try_get("last_error").ok(),
                last_attempt_at: r.try_get("last_attempt_at").ok(),
            })
            .collect();
        if !chans.is_empty() {
            out.push((reminder, chans));
        }
    }
    Ok(out)
}

/// Flip paused reminders whose `paused_until` has elapsed back to
/// `active`. Returns the number of rows resumed.
pub async fn auto_resume_paused(pool: &SqlitePool, now: DateTime<Utc>) -> Result<u64> {
    let res = sqlx::query(
        "UPDATE reminders
            SET status = 'active', paused_until = NULL
          WHERE status = 'paused'
            AND paused_until IS NOT NULL
            AND paused_until <= ?1",
    )
    .bind(now.to_rfc3339())
    .execute(pool)
    .await?;
    Ok(res.rows_affected())
}

/// Record the outcome of one channel attempt for one reminder. Updates
/// the `reminder_channels` row (success → `sent`; failure → bump
/// attempts and possibly flip to `failed`) and appends a
/// `reminder_fires` audit row.
pub async fn record_channel_fire(
    pool: &SqlitePool,
    reminder_id: i64,
    channel: &str,
    fired_at: DateTime<Utc>,
    result: std::result::Result<&str, &str>,
) -> Result<()> {
    let now = fired_at.to_rfc3339();
    let mut tx = pool.begin().await?;

    match result {
        Ok(msg_id) => {
            sqlx::query(
                "UPDATE reminder_channels
                    SET status = 'sent', last_attempt_at = ?1, last_error = NULL
                  WHERE reminder_id = ?2 AND channel = ?3",
            )
            .bind(&now)
            .bind(reminder_id)
            .bind(channel)
            .execute(&mut *tx)
            .await?;

            sqlx::query(
                "INSERT INTO reminder_fires
                    (reminder_id, fired_at, channel, success, msg_id, error)
                 VALUES (?1, ?2, ?3, 1, ?4, NULL)",
            )
            .bind(reminder_id)
            .bind(&now)
            .bind(channel)
            .bind(msg_id)
            .execute(&mut *tx)
            .await?;
        }
        Err(err) => {
            // Bump attempts; flip to `failed` when budget exhausted.
            sqlx::query(
                "UPDATE reminder_channels
                    SET attempts = attempts + 1,
                        last_error = ?1,
                        last_attempt_at = ?2,
                        status = CASE
                                   WHEN attempts + 1 >= ?3 THEN 'failed'
                                   ELSE 'pending'
                                 END
                  WHERE reminder_id = ?4 AND channel = ?5",
            )
            .bind(err)
            .bind(&now)
            .bind(MAX_ATTEMPTS)
            .bind(reminder_id)
            .bind(channel)
            .execute(&mut *tx)
            .await?;

            sqlx::query(
                "INSERT INTO reminder_fires
                    (reminder_id, fired_at, channel, success, msg_id, error)
                 VALUES (?1, ?2, ?3, 0, NULL, ?4)",
            )
            .bind(reminder_id)
            .bind(&now)
            .bind(channel)
            .bind(err)
            .execute(&mut *tx)
            .await?;
        }
    }

    tx.commit().await?;
    Ok(())
}

/// Called after iterating through all of a reminder's pending channels
/// for this tick. If any channel rows are still `pending`, leave
/// `next_fire_at` alone (next tick retries). Once every channel has
/// reached a terminal state (`sent` or `failed`), either:
///   - one-shot: status='fired', next_fire_at=NULL
///   - recurring: recompute next_fire_at from cron, reset channel rows
///                to pending+0 for the next fire
///
/// Wrapped in a single transaction so the channel-reset is atomic with
/// the next_fire_at advance.
pub async fn advance_after_fire(pool: &SqlitePool, reminder_id: i64) -> Result<()> {
    let reminder = load_reminder(pool, reminder_id)
        .await?
        .ok_or_else(|| anyhow!("reminder {reminder_id} vanished mid-fire"))?;

    let pending_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM reminder_channels
          WHERE reminder_id = ?1 AND status = 'pending'",
    )
    .bind(reminder_id)
    .fetch_one(pool)
    .await?;

    let now = Utc::now().to_rfc3339();

    if pending_count > 0 {
        // Still channels to retry; next tick handles them. Don't
        // advance the schedule yet.
        return Ok(());
    }

    let mut tx = pool.begin().await?;

    if reminder.one_shot {
        sqlx::query(
            "UPDATE reminders
                SET status = 'fired', next_fire_at = NULL, last_fired_at = ?1
              WHERE id = ?2",
        )
        .bind(&now)
        .bind(reminder_id)
        .execute(&mut *tx)
        .await?;
    } else {
        let tz = nucleus_tz();
        let next = next_match_utc(&reminder.cron, Utc::now(), tz)?;
        sqlx::query(
            "UPDATE reminders
                SET next_fire_at = ?1, last_fired_at = ?2
              WHERE id = ?3",
        )
        .bind(next.to_rfc3339())
        .bind(&now)
        .bind(reminder_id)
        .execute(&mut *tx)
        .await?;

        // Reset every channel row for the next fire.
        sqlx::query(
            "UPDATE reminder_channels
                SET status = 'pending', attempts = 0, last_error = NULL
              WHERE reminder_id = ?1",
        )
        .bind(reminder_id)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;
    Ok(())
}

pub async fn cancel(pool: &SqlitePool, id: i64) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE reminders
            SET status = 'cancelled', next_fire_at = NULL
          WHERE id = ?1 AND status IN ('active', 'pending', 'paused')",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn pause(
    pool: &SqlitePool,
    id: i64,
    until: Option<DateTime<Utc>>,
) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE reminders
            SET status = 'paused', paused_until = ?1
          WHERE id = ?2 AND status IN ('active', 'pending')",
    )
    .bind(until.map(|d| d.to_rfc3339()))
    .bind(id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

pub async fn resume(pool: &SqlitePool, id: i64) -> Result<bool> {
    let Some(reminder) = load_reminder(pool, id).await? else {
        return Ok(false);
    };
    if reminder.status != "paused" {
        return Ok(false);
    }

    let tz = nucleus_tz();
    // Active vs pending depends on one_shot.
    let new_status = if reminder.one_shot { "pending" } else { "active" };
    // Recompute next_fire_at — the cron might've already passed during
    // the pause; we still want the next future match.
    let next = if reminder.one_shot {
        // For one-shot: if the original next_fire_at is still in the
        // future, keep it; otherwise the pause outlived the one-shot
        // window — fire-late policy says fire on next tick. Use now().
        match reminder
            .next_fire_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        {
            Some(d) if d.with_timezone(&Utc) > Utc::now() => d.with_timezone(&Utc),
            _ => Utc::now(),
        }
    } else {
        next_match_utc(&reminder.cron, Utc::now(), tz)?
    };

    sqlx::query(
        "UPDATE reminders
            SET status = ?1, paused_until = NULL, next_fire_at = ?2
          WHERE id = ?3",
    )
    .bind(new_status)
    .bind(next.to_rfc3339())
    .bind(id)
    .execute(pool)
    .await?;
    Ok(true)
}

#[derive(Debug, Clone)]
pub struct FireLogRow {
    pub id: i64,
    pub reminder_id: i64,
    pub fired_at: String,
    pub channel: String,
    pub success: bool,
    pub msg_id: Option<String>,
    pub error: Option<String>,
}

pub async fn fire_history(
    pool: &SqlitePool,
    days: Option<i64>,
    channel: Option<&str>,
    reminder_id: Option<i64>,
) -> Result<Vec<FireLogRow>> {
    let mut q = String::from(
        "SELECT id, reminder_id, fired_at, channel, success, msg_id, error
           FROM reminder_fires WHERE 1=1",
    );
    let mut binds: Vec<String> = Vec::new();
    if let Some(d) = days {
        let cutoff = (Utc::now() - chrono::Duration::days(d)).to_rfc3339();
        q.push_str(" AND fired_at >= ?");
        binds.push(cutoff);
    }
    if let Some(c) = channel {
        q.push_str(" AND channel = ?");
        binds.push(c.to_string());
    }
    if let Some(rid) = reminder_id {
        q.push_str(" AND reminder_id = ?");
        binds.push(rid.to_string());
    }
    q.push_str(" ORDER BY fired_at DESC LIMIT 500");

    let mut query = sqlx::query(&q);
    for b in &binds {
        query = query.bind(b);
    }
    let rows = query.fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|r| FireLogRow {
            id: r.get("id"),
            reminder_id: r.get("reminder_id"),
            fired_at: r.get("fired_at"),
            channel: r.get("channel"),
            success: r.try_get::<i64, _>("success").unwrap_or(0) != 0,
            msg_id: r.try_get("msg_id").ok(),
            error: r.try_get("error").ok(),
        })
        .collect())
}

/// Idempotent seeding of `created_by = 'system'` reminders. Matches on
/// body so cancelled rows are NOT recreated (a cancelled system row
/// stays cancelled until you delete it or re-add manually). If the row
/// is missing entirely, insert it.
pub async fn seed_default_reminders(pool: &SqlitePool) -> Result<()> {
    struct SeedRow {
        body: &'static str,
        cron: &'static str,
        one_shot: bool,
        channels: &'static [&'static str],
    }
    let seeds: &[SeedRow] = &[SeedRow {
        body: "⏰ End of day — time to log your hours.",
        cron: "30 18 * * 1-5",
        one_shot: false,
        channels: &[CHANNEL_DISCORD_HOME],
    }];

    let tz = nucleus_tz();
    for seed in seeds {
        // Body+system uniqueness — survives cancellation by leaving the
        // cancelled row alone.
        let existing: Option<(i64,)> = sqlx::query_as(
            "SELECT id FROM reminders
              WHERE created_by = 'system' AND body = ?1
              LIMIT 1",
        )
        .bind(seed.body)
        .fetch_optional(pool)
        .await?;
        if existing.is_some() {
            continue;
        }

        let next = next_match_utc(seed.cron, Utc::now(), tz)?;
        let channels: Vec<String> = seed.channels.iter().map(|s| s.to_string()).collect();
        let id = insert_with_channels(
            pool,
            seed.body,
            seed.cron,
            seed.one_shot,
            next,
            &channels,
            "system",
            None,
        )
        .await?;
        tracing::info!(id, body = seed.body, "reminders: seeded system reminder");
    }
    Ok(())
}

// ============ WhatsApp outbound bridge ============
//
// To deliver a reminder to a WhatsApp chat without conflicting with
// Alfred's Baileys auth (single-client constraint), the reminders binary
// enqueues into a table that lives in memory/whatsapp.db. Alfred drains
// it every 5s and sends via its existing socket. See messaging/whatsapp/
// src/db.ts OutboundQueueStore + index.ts startOutboundDrain.

pub async fn open_whatsapp_db(path: &Path) -> Result<SqlitePool> {
    let pool = nucleus_core::db::open(path).await?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS outbound_queue (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            target       TEXT    NOT NULL,
            body         TEXT    NOT NULL,
            source       TEXT    NOT NULL,
            enqueued_at  TEXT    NOT NULL,
            status       TEXT    NOT NULL DEFAULT 'pending',
            attempts     INTEGER NOT NULL DEFAULT 0,
            last_error   TEXT,
            sent_at      TEXT,
            msg_id       TEXT
        );
        "#,
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_outbound_status_enqueued
         ON outbound_queue(status, enqueued_at)",
    )
    .execute(&pool)
    .await?;
    Ok(pool)
}

pub async fn enqueue_whatsapp(
    pool: &SqlitePool,
    target: &str,
    body: &str,
    source: &str,
) -> Result<i64> {
    let now = Utc::now().to_rfc3339();
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO outbound_queue (target, body, source, enqueued_at, status, attempts)
         VALUES (?1, ?2, ?3, ?4, 'pending', 0)
         RETURNING id",
    )
    .bind(target)
    .bind(body)
    .bind(source)
    .bind(&now)
    .fetch_one(pool)
    .await
    .context("enqueue outbound whatsapp")?;
    Ok(row.0)
}

/// Parse an `--at` RFC3339 string into a `DateTime<Tz>` localized to
/// the operator's timezone. Accepts either offset-aware input ("…+00:00")
/// or a naive local timestamp ("2026-05-15T16:45:00") interpreted as
/// `NUCLEUS_TZ` for ergonomics.
pub fn parse_at(at: &str) -> Result<DateTime<Tz>> {
    let tz = nucleus_tz();
    if let Ok(d) = DateTime::parse_from_rfc3339(at) {
        return Ok(d.with_timezone(&tz));
    }
    // Naive local fallback
    let naive = chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%dT%H:%M"))
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M:%S"))
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%d %H:%M"))
        .map_err(|_| {
            anyhow!(
                "--at must be RFC3339 with offset (e.g. 2026-05-14T16:45:00-03:00) or local ISO without offset"
            )
        })?;
    let local = tz
        .from_local_datetime(&naive)
        .single()
        .ok_or_else(|| anyhow!("ambiguous or non-existent local time {naive}"))?;
    Ok(local)
}
