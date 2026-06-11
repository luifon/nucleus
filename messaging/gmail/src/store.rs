//! SQLite store for the gmail-metabolism job (ADR-007).
//!
//! Tables in `memory/gmail.db`:
//!
//! - `killlist_senders` — senders whose mail is auto-trashed on
//!   classification. Seeded manually or auto-promoted by the metabolism
//!   job when a sender hits the configured junk-occurrence threshold.
//! - `sender_hits` — per-sender counter of junk classifications, used
//!   to drive auto-promotion to `killlist_senders`.
//! - `watermark` — single-key K/V; `last_run_at` is the RFC3339 UTC
//!   timestamp of the most recent successful sweep, used to scope the
//!   next run's `newer_than:` Gmail search.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::{Row, SqlitePool};
use std::path::Path;

pub const WATERMARK_KEY: &str = "last_run_at";

pub async fn open(path: &Path) -> Result<SqlitePool> {
    let pool = nucleus_core::db::open(path).await?;
    nucleus_core::migrate::migrate(&pool, MIGRATIONS)
        .await
        .context("migrating gmail.db")?;
    Ok(pool)
}

/// Versioned migrations (ADR-020): v1 = the historical ensure_schema
/// body. New schema changes go in as v2+ and run exactly once.
const MIGRATIONS: &[nucleus_core::migrate::Migration] = &[nucleus_core::migrate::Migration {
    version: 1,
    name: "baseline-adr007",
    step: nucleus_core::migrate::Step::Sql(
        r#"
        CREATE TABLE IF NOT EXISTS killlist_senders (
            email      TEXT PRIMARY KEY,
            added_at   TEXT NOT NULL,
            reason     TEXT,
            added_by   TEXT
        );
        CREATE TABLE IF NOT EXISTS sender_hits (
            email       TEXT PRIMARY KEY,
            junk_hits   INTEGER NOT NULL DEFAULT 0,
            last_hit_at TEXT
        );
        CREATE TABLE IF NOT EXISTS watermark (
            key        TEXT PRIMARY KEY,
            value      TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )
        "#,
    ),
}];

pub async fn read_watermark(pool: &SqlitePool) -> Result<Option<DateTime<Utc>>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT value FROM watermark WHERE key = ?1",
    )
    .bind(WATERMARK_KEY)
    .fetch_optional(pool)
    .await?;
    let Some((value,)) = row else {
        return Ok(None);
    };
    let parsed = DateTime::parse_from_rfc3339(&value)
        .with_context(|| format!("watermark value not RFC3339: {value:?}"))?;
    Ok(Some(parsed.with_timezone(&Utc)))
}

pub async fn write_watermark(pool: &SqlitePool, at: DateTime<Utc>) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO watermark (key, value, updated_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(WATERMARK_KEY)
    .bind(at.to_rfc3339())
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn killlist(pool: &SqlitePool) -> Result<Vec<String>> {
    let rows = sqlx::query("SELECT email FROM killlist_senders ORDER BY added_at DESC")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|r| r.get::<String, _>("email")).collect())
}

pub async fn killlist_add(
    pool: &SqlitePool,
    email: &str,
    reason: Option<&str>,
    added_by: &str,
) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    let res = sqlx::query(
        "INSERT OR IGNORE INTO killlist_senders (email, added_at, reason, added_by)
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(email)
    .bind(now)
    .bind(reason)
    .bind(added_by)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Bump a sender's junk-hit counter and return the new value. Used by
/// the metabolism job to drive auto-promotion.
pub async fn bump_junk_hit(pool: &SqlitePool, email: &str) -> Result<i64> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO sender_hits (email, junk_hits, last_hit_at)
         VALUES (?1, 1, ?2)
         ON CONFLICT(email) DO UPDATE SET
            junk_hits = sender_hits.junk_hits + 1,
            last_hit_at = excluded.last_hit_at",
    )
    .bind(email)
    .bind(now)
    .execute(pool)
    .await?;
    let (n,): (i64,) = sqlx::query_as(
        "SELECT junk_hits FROM sender_hits WHERE email = ?1",
    )
    .bind(email)
    .fetch_one(pool)
    .await?;
    Ok(n)
}
