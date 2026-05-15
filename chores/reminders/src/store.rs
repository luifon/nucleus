//! SQLite store for ad-hoc reminders. Schema is intentionally narrow:
//! one table, no joins, no cascades. The polling worker (`reminders due`)
//! is the only writer of the `fired` / `cancelled` transitions; the CLI
//! `add` / `cancel` are the only writers of `pending`.
//!
//! Channel codes (`channel` column):
//!   - "discord-home"  → DISCORD_HOME_CHANNEL_ID
//!   - "alfred"        → WhatsApp Alfred group (via outbound_queue in
//!                       memory/whatsapp.db; Alfred drains every 5s)
//!   - "braindump"     → WhatsApp Brain Dump group (same path)

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::SqlitePool;
use std::path::Path;

pub const CHANNEL_DISCORD_HOME: &str = "discord-home";
pub const CHANNEL_ALFRED: &str = "alfred";
pub const CHANNEL_BRAINDUMP: &str = "braindump";

pub async fn open(path: &Path) -> Result<SqlitePool> {
    let pool = nucleus_core::db::open(path).await?;
    ensure_schema(&pool).await?;
    Ok(pool)
}

async fn ensure_schema(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS reminders (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            due_at       TEXT    NOT NULL,
            body         TEXT    NOT NULL,
            channel      TEXT    NOT NULL,
            created_at   TEXT    NOT NULL,
            status       TEXT    NOT NULL DEFAULT 'pending',
            fired_at     TEXT,
            fired_msg_id TEXT,
            cancelled_at TEXT
        );
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_reminders_due_status ON reminders(due_at, status)",
    )
    .execute(pool)
    .await?;
    Ok(())
}

#[derive(Debug, sqlx::FromRow, serde::Serialize)]
pub struct Reminder {
    pub id: i64,
    pub due_at: String,
    pub body: String,
    pub channel: String,
    pub created_at: String,
    pub status: String,
    pub fired_at: Option<String>,
    pub fired_msg_id: Option<String>,
    pub cancelled_at: Option<String>,
}

/// Insert a new pending reminder. Returns its id.
pub async fn insert(
    pool: &SqlitePool,
    due_at: DateTime<Utc>,
    body: &str,
    channel: &str,
) -> Result<i64> {
    let now = Utc::now().to_rfc3339();
    let due = due_at.to_rfc3339();
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO reminders (due_at, body, channel, created_at, status)
         VALUES (?1, ?2, ?3, ?4, 'pending')
         RETURNING id",
    )
    .bind(&due)
    .bind(body)
    .bind(channel)
    .bind(&now)
    .fetch_one(pool)
    .await
    .context("insert reminder")?;
    Ok(row.0)
}

/// All currently-pending reminders, ordered by due time.
pub async fn list_pending(pool: &SqlitePool) -> Result<Vec<Reminder>> {
    let rows: Vec<Reminder> = sqlx::query_as(
        "SELECT id, due_at, body, channel, created_at, status,
                fired_at, fired_msg_id, cancelled_at
           FROM reminders
          WHERE status = 'pending'
          ORDER BY due_at ASC",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Reminders that are pending AND due at or before `now`. The polling
/// worker iterates these and tries to deliver each.
pub async fn pending_due(pool: &SqlitePool, now: DateTime<Utc>) -> Result<Vec<Reminder>> {
    let cutoff = now.to_rfc3339();
    let rows: Vec<Reminder> = sqlx::query_as(
        "SELECT id, due_at, body, channel, created_at, status,
                fired_at, fired_msg_id, cancelled_at
           FROM reminders
          WHERE status = 'pending'
            AND due_at <= ?1
          ORDER BY due_at ASC",
    )
    .bind(&cutoff)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn mark_fired(
    pool: &SqlitePool,
    id: i64,
    msg_id: &str,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE reminders
            SET status = 'fired', fired_at = ?1, fired_msg_id = ?2
          WHERE id = ?3 AND status = 'pending'",
    )
    .bind(&now)
    .bind(msg_id)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn cancel(pool: &SqlitePool, id: i64) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    let res = sqlx::query(
        "UPDATE reminders
            SET status = 'cancelled', cancelled_at = ?1
          WHERE id = ?2 AND status = 'pending'",
    )
    .bind(&now)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

// ============ WhatsApp outbound bridge ============
//
// To deliver a reminder to a WhatsApp chat without conflicting with
// Alfred's Baileys auth (single-client constraint), the reminders binary
// enqueues into a table that lives in memory/whatsapp.db. Alfred drains
// it every 5s and sends via its existing socket. See messaging/whatsapp/
// src/db.ts OutboundQueueStore + index.ts startOutboundDrain.

/// Open the WhatsApp DB (or create it if Alfred has never run). The
/// outbound_queue schema is declared idempotently here so the reminders
/// binary can enqueue even on a fresh install where Alfred hasn't
/// initialized the DB yet.
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

/// Enqueue a message for Alfred to deliver. `target` is either a group
/// name ("Alfred", "Brain Dump") or a JID — Alfred's drainer resolves
/// against its allowlist and refuses unknown targets.
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
