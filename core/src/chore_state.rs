//! Chore state (ADR-029): the small, durable facts a maintenance job needs
//! from one run to the next, kept out of the jobs' own code.
//!
//! Core owns `memory/chore_state.db` (ADR-020 DB-ownership). Two tables:
//!
//! - `daily_sessions` — one claude session per local day per key. A
//!   high-frequency or multi-pass job (the */30 heartbeat, the distiller's
//!   metabolism + contemplation, the skill-gap-learner's review + learn arms)
//!   resumes the day's session instead of spawning a fresh transcript every
//!   time. Wired through [`crate::session_profile::SessionProfile::daily_session`].
//! - `watermarks` — the last point a job processed, so a run that failed
//!   (or a machine that was off) is caught up by the next run instead of
//!   being skipped. Values are opaque strings; date-keyed jobs store
//!   `YYYY-MM-DD`.
//!
//! Losing this file costs one extra transcript and one wider catch-up
//! window — it is never backed up.

use anyhow::Result;
use sqlx::SqlitePool;
use std::path::Path;

pub const DB_PATH: &str = "memory/chore_state.db";

const MIGRATIONS: &[crate::migrate::Migration] = &[crate::migrate::Migration {
    version: 1,
    name: "adr029-chore-state",
    step: crate::migrate::Step::Sql(
        "CREATE TABLE IF NOT EXISTS daily_sessions (
            key          TEXT PRIMARY KEY,
            session_date TEXT NOT NULL,
            session_id   TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS watermarks (
            key        TEXT PRIMARY KEY,
            value      TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )",
    ),
}];

pub async fn open(workspace_root: &Path) -> Result<SqlitePool> {
    let pool = crate::db::open(&workspace_root.join(DB_PATH)).await?;
    crate::migrate::migrate(&pool, MIGRATIONS).await?;
    Ok(pool)
}

/// Today's date in the local timezone, the key every daily-session lookup
/// uses. One helper so callers and the profile agree on the boundary.
pub fn today_local() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// The claude session recorded for `key` on `date`, if any. A row for a
/// different date is stale and reads as `None`.
pub async fn daily_session(workspace_root: &Path, key: &str, date: &str) -> Result<Option<String>> {
    let pool = open(workspace_root).await?;
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT session_id FROM daily_sessions WHERE key = ?1 AND session_date = ?2",
    )
    .bind(key)
    .bind(date)
    .fetch_optional(&pool)
    .await?;
    Ok(row.map(|(s,)| s))
}

/// Record the session `key` used on `date`. One row per key, overwritten
/// when the date rolls — the next day's first spawn finds no match and
/// starts fresh.
pub async fn set_daily_session(
    workspace_root: &Path,
    key: &str,
    date: &str,
    session_id: &str,
) -> Result<()> {
    let pool = open(workspace_root).await?;
    sqlx::query(
        "INSERT INTO daily_sessions (key, session_date, session_id) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET
             session_date = excluded.session_date,
             session_id   = excluded.session_id",
    )
    .bind(key)
    .bind(date)
    .bind(session_id)
    .execute(&pool)
    .await?;
    Ok(())
}

/// Forget the session for `key`. Used when resuming it failed to boot, so
/// the day continues on a fresh session rather than retrying a dead one.
pub async fn clear_daily_session(workspace_root: &Path, key: &str) -> Result<()> {
    let pool = open(workspace_root).await?;
    sqlx::query("DELETE FROM daily_sessions WHERE key = ?1")
        .bind(key)
        .execute(&pool)
        .await?;
    Ok(())
}

pub async fn watermark(workspace_root: &Path, key: &str) -> Result<Option<String>> {
    let pool = open(workspace_root).await?;
    let row: Option<(String,)> = sqlx::query_as("SELECT value FROM watermarks WHERE key = ?1")
        .bind(key)
        .fetch_optional(&pool)
        .await?;
    Ok(row.map(|(v,)| v))
}

pub async fn set_watermark(workspace_root: &Path, key: &str, value: &str) -> Result<()> {
    let pool = open(workspace_root).await?;
    sqlx::query(
        "INSERT INTO watermarks (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET
             value      = excluded.value,
             updated_at = excluded.updated_at",
    )
    .bind(key)
    .bind(value)
    .bind(chrono::Utc::now().to_rfc3339())
    .execute(&pool)
    .await?;
    Ok(())
}

/// The first local date a date-keyed job must process: the day after its
/// watermark, or `default_days_back` days ago when it has never run (or the
/// watermark is unparsable). Never later than today.
pub async fn resume_date_after(
    workspace_root: &Path,
    key: &str,
    default_days_back: i64,
) -> Result<chrono::NaiveDate> {
    let today = chrono::Local::now().date_naive();
    let fallback = today - chrono::Duration::days(default_days_back);
    let from = match watermark(workspace_root, key).await? {
        Some(v) => match v.parse::<chrono::NaiveDate>() {
            Ok(d) => d.succ_opt().unwrap_or(today),
            Err(_) => {
                tracing::warn!(key, value = %v, "watermark is not a date — using the default window");
                fallback
            }
        },
        None => fallback,
    };
    Ok(from.min(today))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "chore-state-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[tokio::test]
    async fn daily_session_is_scoped_to_its_date() {
        let root = tmp_root();
        set_daily_session(&root, "job", "2026-09-07", "sid-1").await.unwrap();
        assert_eq!(
            daily_session(&root, "job", "2026-09-07").await.unwrap().as_deref(),
            Some("sid-1")
        );
        assert_eq!(daily_session(&root, "job", "2026-09-08").await.unwrap(), None);
        // A new day overwrites the single row.
        set_daily_session(&root, "job", "2026-09-08", "sid-2").await.unwrap();
        assert_eq!(daily_session(&root, "job", "2026-09-07").await.unwrap(), None);
        assert_eq!(
            daily_session(&root, "job", "2026-09-08").await.unwrap().as_deref(),
            Some("sid-2")
        );
        clear_daily_session(&root, "job").await.unwrap();
        assert_eq!(daily_session(&root, "job", "2026-09-08").await.unwrap(), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn resume_date_follows_the_watermark_and_defaults_when_absent() {
        let root = tmp_root();
        let today = chrono::Local::now().date_naive();
        assert_eq!(
            resume_date_after(&root, "job", 1).await.unwrap(),
            today - chrono::Duration::days(1)
        );
        let five_ago = today - chrono::Duration::days(5);
        set_watermark(&root, "job", &five_ago.to_string()).await.unwrap();
        assert_eq!(
            resume_date_after(&root, "job", 1).await.unwrap(),
            five_ago + chrono::Duration::days(1)
        );
        // A watermark of today (or in the future) clamps to today.
        set_watermark(&root, "job", &today.to_string()).await.unwrap();
        assert_eq!(resume_date_after(&root, "job", 1).await.unwrap(), today);
        // Garbage falls back to the default window instead of failing.
        set_watermark(&root, "job", "not-a-date").await.unwrap();
        assert_eq!(
            resume_date_after(&root, "job", 3).await.unwrap(),
            today - chrono::Duration::days(3)
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
