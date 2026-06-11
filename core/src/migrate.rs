//! Versioned SQLite migrations (ADR-020).
//!
//! Replaces the per-binary "ensure_schema on every boot" pattern — which
//! could never express *run exactly once*, so every data-moving step
//! (backfills, column drops, value sweeps) re-executed forever — with a
//! `schema_migrations` ledger: each migration runs once, is recorded, and
//! later boots skip it.
//!
//! Baseline convention: each DB's **v1 is its pre-runner `ensure_schema`
//! body verbatim** (CREATE IF NOT EXISTS + tolerated ALTERs + backfills) —
//! intentionally idempotent, so existing DBs that already carry the full
//! schema run it once as a no-op and get the baseline row, while fresh DBs
//! are built by it. From v2 onward migrations are plain, non-idempotent,
//! run-once steps.

use anyhow::{Context, Result, anyhow};
use futures::future::BoxFuture;
use sqlx::SqlitePool;

/// One migration step.
pub enum Step {
    /// One or more `;`-separated SQL statements, applied atomically:
    /// every statement plus the version row run inside a single
    /// `BEGIN IMMEDIATE` transaction — a crash leaves either nothing or
    /// everything. (Statement splitting is naive — don't embed `;` inside
    /// string literals or triggers; use `Rust` for anything exotic.)
    Sql(&'static str),
    /// Arbitrary async Rust against the pool. Runs OUTSIDE the runner's
    /// transaction (it may need multiple connections / its own
    /// transactions; nesting it under a held write-lock would deadlock
    /// against the pool). The version row is recorded only after it
    /// succeeds, so a crash mid-step re-runs it on the next boot:
    /// a `Rust` step MUST therefore be idempotent or internally
    /// transactional. The v1 baselines satisfy this by construction.
    Rust(for<'a> fn(&'a SqlitePool) -> BoxFuture<'a, Result<()>>),
}

pub struct Migration {
    /// Strictly increasing per DB, starting at 1.
    pub version: i64,
    pub name: &'static str,
    pub step: Step,
}

/// Apply all unapplied migrations in order; returns how many ran.
///
/// Concurrency: two processes booting at once serialize on SQLite's write
/// lock (`BEGIN IMMEDIATE` + the pool's 5s busy_timeout); the loser
/// re-checks the ledger inside its transaction and skips. `Rust` steps
/// race benignly (both may run; idempotency absorbs it; `INSERT OR
/// IGNORE` records one row).
pub async fn migrate(pool: &SqlitePool, migrations: &[Migration]) -> Result<usize> {
    for w in migrations.windows(2) {
        if w[1].version <= w[0].version {
            return Err(anyhow!(
                "migrations must be strictly increasing: v{} `{}` follows v{} `{}`",
                w[1].version,
                w[1].name,
                w[0].version,
                w[0].name
            ));
        }
    }

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version    INTEGER PRIMARY KEY,
            name       TEXT NOT NULL,
            applied_at TEXT NOT NULL
        )",
    )
    .execute(pool)
    .await
    .context("creating schema_migrations")?;

    let mut applied = 0usize;
    for m in migrations {
        let done: Option<i64> =
            sqlx::query_scalar("SELECT version FROM schema_migrations WHERE version = ?1")
                .bind(m.version)
                .fetch_optional(pool)
                .await?;
        if done.is_some() {
            continue;
        }

        match &m.step {
            Step::Sql(sql) => {
                let mut conn = pool.acquire().await?;
                sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
                // Re-check inside the write transaction — closes the race
                // where another process applied it between our check and
                // our lock acquisition.
                let dup: Option<i64> = sqlx::query_scalar(
                    "SELECT version FROM schema_migrations WHERE version = ?1",
                )
                .bind(m.version)
                .fetch_optional(&mut *conn)
                .await?;
                if dup.is_some() {
                    sqlx::query("ROLLBACK").execute(&mut *conn).await?;
                    continue;
                }
                let result = async {
                    for stmt in sql.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                        sqlx::query(stmt).execute(&mut *conn).await.with_context(|| {
                            format!("migration v{} `{}` failed on: {stmt}", m.version, m.name)
                        })?;
                    }
                    sqlx::query(
                        "INSERT INTO schema_migrations (version, name, applied_at)
                         VALUES (?1, ?2, ?3)",
                    )
                    .bind(m.version)
                    .bind(m.name)
                    .bind(chrono::Utc::now().to_rfc3339())
                    .execute(&mut *conn)
                    .await?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                match result {
                    Ok(()) => {
                        sqlx::query("COMMIT").execute(&mut *conn).await?;
                    }
                    Err(e) => {
                        let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                        return Err(e);
                    }
                }
            }
            Step::Rust(f) => {
                f(pool)
                    .await
                    .with_context(|| format!("migration v{} `{}`", m.version, m.name))?;
                sqlx::query(
                    "INSERT OR IGNORE INTO schema_migrations (version, name, applied_at)
                     VALUES (?1, ?2, ?3)",
                )
                .bind(m.version)
                .bind(m.name)
                .bind(chrono::Utc::now().to_rfc3339())
                .execute(pool)
                .await?;
            }
        }

        tracing::info!(version = m.version, name = m.name, "applied migration");
        applied += 1;
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn mem_pool() -> SqlitePool {
        SqlitePool::connect("sqlite::memory:").await.unwrap()
    }

    fn v1_sql() -> Migration {
        Migration {
            version: 1,
            name: "baseline",
            step: Step::Sql(
                "CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY, v TEXT);
                 CREATE INDEX IF NOT EXISTS idx_t_v ON t(v)",
            ),
        }
    }

    #[tokio::test]
    async fn fresh_db_applies_all_then_reruns_zero() {
        let pool = mem_pool().await;
        let migrations = [
            v1_sql(),
            Migration {
                version: 2,
                name: "add-col",
                step: Step::Sql("ALTER TABLE t ADD COLUMN extra TEXT"),
            },
        ];
        assert_eq!(migrate(&pool, &migrations).await.unwrap(), 2);
        assert_eq!(migrate(&pool, &migrations).await.unwrap(), 0, "second run is a no-op");
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schema_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 2);
    }

    #[tokio::test]
    async fn rust_step_runs_and_is_recorded() {
        fn step(pool: &SqlitePool) -> BoxFuture<'_, Result<()>> {
            Box::pin(async move {
                sqlx::query("CREATE TABLE IF NOT EXISTS r (id INTEGER PRIMARY KEY)")
                    .execute(pool)
                    .await?;
                Ok(())
            })
        }
        let pool = mem_pool().await;
        let migrations = [Migration { version: 1, name: "rust-baseline", step: Step::Rust(step) }];
        assert_eq!(migrate(&pool, &migrations).await.unwrap(), 1);
        assert_eq!(migrate(&pool, &migrations).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn failed_sql_migration_rolls_back_atomically() {
        let pool = mem_pool().await;
        let migrations = [Migration {
            version: 1,
            name: "broken",
            step: Step::Sql("CREATE TABLE good (id INTEGER); SYNTAX ERROR HERE"),
        }];
        assert!(migrate(&pool, &migrations).await.is_err());
        // The CREATE before the failure must have been rolled back.
        let exists: Option<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='good'",
        )
        .fetch_optional(&pool)
        .await
        .unwrap();
        assert!(exists.is_none(), "partial migration must roll back");
        // And nothing recorded — it retries next boot.
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM schema_migrations")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[tokio::test]
    async fn non_increasing_versions_rejected() {
        let pool = mem_pool().await;
        let migrations = [
            Migration { version: 2, name: "b", step: Step::Sql("SELECT 1") },
            Migration { version: 2, name: "dup", step: Step::Sql("SELECT 1") },
        ];
        assert!(migrate(&pool, &migrations).await.is_err());
    }
}
