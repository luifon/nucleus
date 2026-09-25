//! The common event record and the source adapter interface (ADR-036).
//!
//! A source adapter converts what its source reports (a GitHub issue, an
//! email, an alert) into a [`NewEvent`]. The core never looks at a source's
//! own format again: it stores the event, keeps the original payload in
//! `raw` for audit, and decides on the pipeline from the common fields and
//! the adapter's gate decision (`accepted`).
//!
//! Adding a source means implementing [`SourceAdapter`] (polling sources) or
//! calling `nucleus events emit` (scripts, push-style sources); the store,
//! the dedup and the pipeline stay the same.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// One event as an adapter reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct NewEvent {
    /// Adapter name: `github`, `cli`, or any name a script uses.
    pub source: String,
    /// Identity of the item inside its source; `(source, external_id)` is
    /// unique. GitHub: `owner/name#12`.
    pub external_id: String,
    /// The repository or project the event is about (`owner/name`), when
    /// the source knows it. Only events on a configured repo can become
    /// pipeline items.
    pub project: Option<String>,
    /// `issue`, `alert`, `message`, …
    pub kind: String,
    pub title: String,
    pub body: String,
    /// The source's name for the author. Never trusted: anyone can open an
    /// issue.
    pub author: Option<String>,
    pub labels: Vec<String>,
    pub url: Option<String>,
    /// `open` or `closed`.
    pub state: String,
    /// When the source created / last changed the item (RFC3339).
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    /// The source's own record, unchanged.
    pub raw: serde_json::Value,
    /// The adapter's gate decision: this event may start pipeline work.
    /// GitHub: the issue carries the configured label.
    pub accepted: bool,
}

/// A stored event (`events` table).
#[derive(Debug, Clone, Serialize, Deserialize, ts_rs::TS)]
#[ts(export, rename = "IntakeEvent")]
pub struct Event {
    #[ts(type = "number")]
    pub id: i64,
    pub source: String,
    pub external_id: String,
    pub project: Option<String>,
    pub kind: String,
    pub title: String,
    pub body: String,
    pub author: Option<String>,
    pub labels: Vec<String>,
    pub url: Option<String>,
    pub state: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub accepted: bool,
    pub first_seen_at: String,
    pub last_seen_at: String,
}

/// One comment of the discussion on an event, from a trusted author.
#[derive(Debug, Clone, PartialEq)]
pub struct Comment {
    pub author: String,
    pub body: String,
    pub created_at: String,
}

/// The discussion on an event as the pipeline may use it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Discussion {
    /// Comments whose authors the source trusts (GitHub: repo
    /// collaborators), oldest first.
    pub trusted: Vec<Comment>,
    /// Comments left out because their authors are not trusted.
    pub ignored: usize,
}

/// What one poll returned.
#[derive(Debug, Clone, Default)]
pub struct PollBatch {
    pub events: Vec<NewEvent>,
    /// The cursor to store once every event of the batch is recorded; the
    /// next poll starts from it. `None` keeps the previous cursor.
    pub next_cursor: Option<String>,
}

/// A polling source.
///
/// The core calls [`SourceAdapter::poll`] with the cursor it stored after
/// the previous poll (an ADR-029 watermark under
/// [`SourceAdapter::cursor_key`]); the adapter returns every event that
/// changed since then. Returning an event again is harmless: the store
/// updates the existing record.
#[async_trait::async_trait]
pub trait SourceAdapter: Send + Sync {
    /// The `source` value of this adapter's events.
    fn source(&self) -> &str;
    /// The watermark key of this adapter instance (one per repo for
    /// GitHub).
    fn cursor_key(&self) -> String;
    /// Minimum time between two polls.
    fn poll_interval(&self) -> Duration;
    /// Events changed since `cursor` (`None`: the first poll).
    async fn poll(&self, cursor: Option<&str>) -> Result<PollBatch>;
    /// The trusted part of the discussion on `event`.
    async fn discussion(&self, event: &Event) -> Result<Discussion>;
    /// Post `body` on the event at its source (GitHub: an issue comment).
    /// `marker` is a unique string the adapter embeds invisibly and checks
    /// first, so a retry after a crash does not post twice. Returns the
    /// URL of the reply when the source gives one.
    async fn reply(&self, event: &Event, body: &str, marker: &str) -> Result<Option<String>>;
}
