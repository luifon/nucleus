use async_trait::async_trait;
use chrono::{DateTime, Utc};
use nucleus_core::health::{HealthCheck, Snapshot, Status};
use sqlx::SqlitePool;

pub struct FetcherCheck {
    id: String,
    pool: Option<SqlitePool>,
}

impl FetcherCheck {
    pub fn new(pool: Option<SqlitePool>) -> Self {
        Self { id: "news-fetcher".into(), pool }
    }
}

#[async_trait]
impl HealthCheck for FetcherCheck {
    fn id(&self) -> &str { &self.id }

    async fn probe(&self) -> Snapshot {
        let now = Utc::now();
        let Some(pool) = &self.pool else {
            return Snapshot {
                id: self.id.clone(),
                status: Status::Unknown,
                message: Some("news.db not opened".into()),
                checked_at: now,
            };
        };
        let row: Option<(String, Option<String>, i64, i64, i64)> = sqlx::query_as(
            "SELECT started_at, finished_at, items_new, items_notable, ok FROM fetcher_runs ORDER BY started_at DESC LIMIT 1"
        ).fetch_optional(pool).await.ok().flatten();
        match row {
            None => Snapshot {
                id: self.id.clone(),
                status: Status::Unknown,
                message: Some("no runs recorded".into()),
                checked_at: now,
            },
            Some((started, finished, new, notable, ok)) => {
                let ago = DateTime::parse_from_rfc3339(&started)
                    .map(|t| (now - t.with_timezone(&Utc)).num_minutes())
                    .unwrap_or(i64::MAX);
                let status = match () {
                    _ if ok != 1 => Status::Degraded,
                    _ if ago > 60 * 24 => Status::Degraded, // >24h since last run
                    _ => Status::Ok,
                };
                let msg = format!(
                    "last run {} min ago — {} new, {} notable, finished={}",
                    ago, new, notable, finished.is_some()
                );
                Snapshot { id: self.id.clone(), status, message: Some(msg), checked_at: now }
            }
        }
    }
}
