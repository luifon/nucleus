use async_trait::async_trait;
use chrono::Utc;
use nucleus_core::health::{HealthCheck, Snapshot, Status};

/// Trivially-OK self check — proves the registry can probe at all.
pub struct SelfCheck;

#[async_trait]
impl HealthCheck for SelfCheck {
    fn id(&self) -> &str { "dashboard" }

    async fn probe(&self) -> Snapshot {
        Snapshot {
            id: "dashboard".into(),
            status: Status::Ok,
            message: Some("axum running".into()),
            checked_at: Utc::now(),
        }
    }
}
