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
    /// Why the last gate check created no item (GitHub: the label was added
    /// by a non-collaborator, the text changed after the label, …).
    pub gate_note: Option<String>,
}

/// One comment of the discussion on an event, from a trusted author.
#[derive(Debug, Clone, PartialEq)]
pub struct Comment {
    /// The comment's id at its source (the pipeline binds an item to the
    /// content of every comment it used, by id).
    pub id: String,
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

/// Who opened an item's gate, as the source records it (GitHub: the
/// timeline event that added the label, and a later reopen).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateEvidence {
    /// The event that opened the gate: the label event, or a reopen after
    /// it. A new gate event starts a new item.
    pub event_id: String,
    /// The event that added the gate label.
    pub label_event_id: String,
    /// The account that added the label.
    pub label_actor: String,
    /// When the label was added (RFC3339).
    pub label_at: String,
    /// The account behind `event_id` (the label actor, or who reopened).
    pub opener: String,
    /// True when `label_actor` and `opener` are trusted by the source
    /// (GitHub: repository collaborators), checked live, never from a cache.
    pub trusted: bool,
}

/// The source's current state of an event, read live (never from the
/// store or a cache). The pipeline reads it when it admits an event and
/// right before every irreversible step.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceState {
    pub open: bool,
    /// The gate label is present.
    pub accepted: bool,
    pub title: String,
    pub body: String,
    /// `None` when the source has no record of who set the label.
    pub gate: Option<GateEvidence>,
    /// The title or body was edited after the label was added.
    pub edited_after_gate: bool,
    /// The source's last-change time of the event (GitHub: `updated_at`).
    pub updated_at: Option<String>,
    /// When the body was last edited (GitHub: GraphQL `lastEditedAt`).
    pub last_edited_at: Option<String>,
}

/// The content hash an item is bound to: title and body of the event at
/// the moment its gate was satisfied (SHA-256, hex).
pub fn revision_hash(title: &str, body: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(title.as_bytes());
    h.update([0u8]);
    h.update(body.as_bytes());
    hex(&h.finalize())
}

/// SHA-256 of one comment's body, hex.
pub fn comment_hash(body: &str) -> String {
    use sha2::{Digest, Sha256};
    hex(&Sha256::digest(body.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
    /// The trusted part of the discussion on `event`. With `live`, every
    /// author's trust is checked at the source now; otherwise a cached
    /// answer may be used (display and polling only).
    async fn discussion(&self, event: &Event, live: bool) -> Result<Discussion>;
    /// The event's state at the source now, with the evidence of who opened
    /// its gate. Any failure is an error; the caller fails closed.
    async fn live_state(&self, event: &Event) -> Result<SourceState>;
    /// Post `body` on the event at its source (GitHub: an issue comment).
    /// `marker` is a unique string the adapter embeds invisibly and checks
    /// first, so a retry after a crash does not post twice. Returns the
    /// URL of the reply when the source gives one.
    async fn reply(&self, event: &Event, body: &str, marker: &str) -> Result<Option<String>>;
}
