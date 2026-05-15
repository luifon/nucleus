//! Health check trait + registry. Used heavily by `dashboard`; other binaries
//! can self-report. See ADR-001 for the dashboard surface.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ok,
    Degraded,
    Down,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: String,
    pub status: Status,
    pub message: Option<String>,
    pub checked_at: DateTime<Utc>,
}

#[async_trait]
pub trait HealthCheck: Send + Sync {
    fn id(&self) -> &str;
    async fn probe(&self) -> Snapshot;
}

#[derive(Default)]
pub struct Registry {
    checks: Vec<Box<dyn HealthCheck>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register<C: HealthCheck + 'static>(&mut self, c: C) -> &mut Self {
        self.checks.push(Box::new(c));
        self
    }

    pub async fn snapshot(&self) -> Vec<Snapshot> {
        let mut out = Vec::with_capacity(self.checks.len());
        for c in &self.checks {
            out.push(c.probe().await);
        }
        out
    }
}
