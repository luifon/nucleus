use async_trait::async_trait;
use chrono::Utc;
use nucleus_core::health::{HealthCheck, Snapshot, Status};
use std::path::PathBuf;

pub struct HermesCheck {
    id: String,
    pid_file: PathBuf,
}

impl HermesCheck {
    pub fn new() -> Self {
        let home = std::env::var("HOME").unwrap_or_default();
        Self {
            id: "hermes-gateway".into(),
            pid_file: PathBuf::from(home).join(".hermes/gateway.pid"),
        }
    }
}

#[async_trait]
impl HealthCheck for HermesCheck {
    fn id(&self) -> &str { &self.id }

    async fn probe(&self) -> Snapshot {
        let now = Utc::now();
        let raw = match std::fs::read_to_string(&self.pid_file) {
            Ok(s) => s.trim().to_string(),
            Err(_) => {
                return Snapshot {
                    id: self.id.clone(),
                    status: Status::Down,
                    message: Some("no pid file (gateway stopped or never started)".into()),
                    checked_at: now,
                };
            }
        };
        // Hermes writes JSON: {"pid": 78485, ...}. Older versions wrote a bare integer.
        let pid: i32 = serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| v.get("pid").and_then(|p| p.as_i64()).map(|p| p as i32))
            .unwrap_or_else(|| raw.parse().unwrap_or(0));
        if pid == 0 {
            return Snapshot {
                id: self.id.clone(),
                status: Status::Down,
                message: Some(format!("invalid pid '{}'", raw)),
                checked_at: now,
            };
        }
        // kill -0 = signal 0 = "is process alive?"
        let alive = unsafe { libc_kill(pid, 0) } == 0;
        if alive {
            Snapshot {
                id: self.id.clone(),
                status: Status::Ok,
                message: Some(format!("running (pid {})", pid)),
                checked_at: now,
            }
        } else {
            Snapshot {
                id: self.id.clone(),
                status: Status::Down,
                message: Some(format!("pid {} not running (stale pid file)", pid)),
                checked_at: now,
            }
        }
    }
}

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

unsafe fn libc_kill(pid: i32, sig: i32) -> i32 {
    kill(pid, sig)
}
