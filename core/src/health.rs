//! Health `Snapshot`/`Status` wire types: the dashboard produces them (via its
//! own checks) and the discord bot renders them over HTTP (ADR-001).

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
