//! sqlx pool helpers. Each binary owns its own SQLite file under `nucleus/memory/`.

use anyhow::Result;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool};
use std::path::Path;
use std::str::FromStr;

/// Open (or create) a SQLite DB at `path`. Parent dirs are created if needed.
/// WAL mode + foreign keys on by default.
///
/// `busy_timeout`: several independent processes share files under
/// `memory/` (reminders-tick writes into whatsapp.db's outbound_queue while
/// the TS bot drains it; the dashboard reads everyone's DBs). SQLite's
/// default is fail-fast on a held write lock — 5s of retry absorbs any
/// realistic cross-process contention at this scale (ADR-020).
pub async fn open(path: &Path) -> Result<SqlitePool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(5));
    let pool = SqlitePool::connect_with(opts).await?;
    Ok(pool)
}

/// Open an EXISTING SQLite DB read-only; errors if it's missing. For DBs
/// owned by another process family (ADR-020 ownership — e.g. documents.db
/// is TS-written): `open()`'s create_if_missing would silently create an
/// empty foreign-owned DB and mask "not initialized". WAL reads from the
/// same user work; busy_timeout absorbs the owner's writes.
pub async fn open_read_only(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .read_only(true)
        .busy_timeout(std::time::Duration::from_secs(5));
    let pool = SqlitePool::connect_with(opts).await?;
    Ok(pool)
}
