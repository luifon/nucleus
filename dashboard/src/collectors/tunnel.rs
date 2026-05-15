use async_trait::async_trait;
use chrono::Utc;
use nucleus_core::health::{HealthCheck, Snapshot, Status};

/// Probe a tunnel-fronted URL. 2xx/3xx → Ok, other HTTP code → Degraded,
/// network error → Down.
pub struct TunnelCheck {
    id: String,
    url: String,
}

impl TunnelCheck {
    pub fn new(id: impl Into<String>, url: impl Into<String>) -> Self {
        Self { id: id.into(), url: url.into() }
    }
}

#[async_trait]
impl HealthCheck for TunnelCheck {
    fn id(&self) -> &str { &self.id }

    async fn probe(&self) -> Snapshot {
        let now = Utc::now();
        let client = match reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(e) => {
                return Snapshot {
                    id: self.id.clone(),
                    status: Status::Unknown,
                    message: Some(format!("client: {}", e)),
                    checked_at: now,
                };
            }
        };
        match client.get(&self.url).send().await {
            Ok(r) => {
                let code = r.status().as_u16();
                let status = if (200..=399).contains(&code) {
                    Status::Ok
                } else {
                    Status::Degraded
                };
                Snapshot {
                    id: self.id.clone(),
                    status,
                    message: Some(format!("HTTP {} from {}", code, self.url)),
                    checked_at: now,
                }
            }
            Err(e) => Snapshot {
                id: self.id.clone(),
                status: Status::Down,
                message: Some(format!("{}: {}", self.url, e)),
                checked_at: now,
            },
        }
    }
}
