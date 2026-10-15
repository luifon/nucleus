//! Transcript turn tracker (ADR-033).
//!
//! Reads the Claude Code transcript JSONL incrementally and reports what the
//! session is doing in terms a caller can act on: a prompt was accepted, a
//! message typed during a running turn was absorbed, the model wrote
//! intermediate text before a tool call, a background task started, finished
//! or never started, the session was compacted, and the turn ended.
//!
//! Why this exists: `Session::ask` returns the first assistant text followed by
//! a quiet transcript (or the first `end_turn`). Both are wrong for long
//! conversational work. The first returns narration written before a slow tool
//! call as the answer. The second misses turns that the session starts on its
//! own: when the model runs a command in the background, it ends its turn with
//! "I will report when it finishes", and the real answer arrives in a later
//! turn that starts from a `task-notification`. The tracker follows every turn,
//! so a caller can deliver each final answer exactly once.
//!
//! Record shapes this relies on (Claude Code 2.1.28x, verified 2026-09-24 with
//! a live session; see `core/testdata/turn_tracker_vectors.json`):
//!
//! - `{"type":"user","origin":{"kind":"human"},"message":{"content":"…"}}` —
//!   a typed prompt that starts a turn. `origin.kind` is `task-notification`
//!   for a turn the session starts itself when a background task finishes.
//! - `{"type":"attachment","attachment":{"type":"queued_command","prompt":"…"}}`
//!   — input typed while a turn was running, absorbed at the next step. It
//!   does not end the turn.
//! - `{"type":"queue-operation","operation":"enqueue","content":"…"}` — input
//!   accepted into the queue while the session was busy.
//! - `{"type":"assistant","message":{"id":…,"stop_reason":…,"content":[…]}}` —
//!   one record per content block; all records of one message share `id`.
//! - `{"type":"system","subtype":"turn_duration"}` — the turn is over.
//! - `{"type":"system","subtype":"compact_boundary","compactMetadata":{"trigger":…}}`
//!   followed by a `user` record with `isCompactSummary` and
//!   `isVisibleInTranscriptOnly` — the context was compacted (`/compact` or
//!   the automatic compaction). The summary record is not a prompt; no
//!   `turn_duration` follows a compaction outside a turn (Claude Code 2.1.300,
//!   verified 2026-10-15).
//! - A slash command typed into the session is a `user` record with the
//!   command line and no `origin`, then records starting `<command-name>` and
//!   `<local-command-stdout>`. A local command writes no `turn_duration`, so
//!   none of these is a prompt.
//! - A `tool_result` with `is_error: true` for a background tool call: the
//!   call was refused (permission classifier, operator) and never started.
//!
//! The TypeScript mirror is `messaging/whatsapp/src/turn_tracker.ts`; both run
//! the shared vectors.

use serde_json::Value;
use std::collections::BTreeSet;

/// One thing the tracker observed. Serialized with the same field names the
/// shared test vectors use.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TrackEvent {
    /// A user prompt record. `starts_turn` is false when a turn was already
    /// open (rare: the harness normally turns mid-turn input into
    /// `absorbed`).
    Prompt { origin: String, text: String, starts_turn: bool },
    /// Input typed while a turn was running, consumed by that turn.
    Absorbed { text: String },
    /// Input accepted into the harness queue (the session was busy).
    Enqueued { text: String },
    /// Assistant text written before a tool call, inside a running turn.
    Progress { text: String },
    /// A tool call that runs in the background (`run_in_background: true`).
    /// `description` is what the call says it runs (see [`bg_description`]).
    BgStarted { id: String, description: Option<String> },
    /// The harness reported a background task as finished.
    BgFinished { id: String },
    /// A background tool call whose result was an error: it never started.
    BgNeverStarted { id: String },
    /// The session's context was compacted (`compact_boundary`). `trigger`
    /// is `manual` for `/compact`, `auto` for the automatic compaction.
    Compacted { trigger: String },
    /// The turn ended. `final_text` is the text of the turn's last assistant
    /// message, or `None` when that message had no text (an interrupted
    /// turn). `pending_bg` counts background tasks still running: the model
    /// ended its turn while waiting for them, and another turn will follow.
    TurnEnd { final_text: Option<String>, pending_bg: usize },
}

/// Incremental parser. Feed it raw transcript bytes in order; it buffers a
/// partial trailing line until the rest arrives.
#[derive(Debug, Default)]
pub struct TurnTracker {
    partial: String,
    open: bool,
    /// Message id of the most recent assistant message in the open turn.
    last_msg_id: Option<String>,
    /// Text accumulated for `last_msg_id`.
    last_msg_text: String,
    pending_bg: BTreeSet<String>,
}

impl TurnTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// True while a turn has started and not yet ended.
    pub fn turn_open(&self) -> bool {
        self.open
    }

    /// Background tasks started and not yet reported finished.
    pub fn pending_bg(&self) -> usize {
        self.pending_bg.len()
    }

    /// Feed newly appended transcript bytes; returns the events they produced.
    pub fn feed(&mut self, chunk: &str) -> Vec<TrackEvent> {
        self.partial.push_str(chunk);
        let mut out = Vec::new();
        while let Some(nl) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=nl).collect();
            self.line(line.trim(), &mut out);
        }
        out
    }

    /// Feed one complete record (test helper and whole-file reads).
    pub fn feed_record(&mut self, record: &Value) -> Vec<TrackEvent> {
        let mut out = Vec::new();
        self.record(record, &mut out);
        out
    }

    fn line(&mut self, line: &str, out: &mut Vec<TrackEvent>) {
        if line.is_empty() {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            return;
        };
        self.record(&v, out);
    }

    fn record(&mut self, v: &Value, out: &mut Vec<TrackEvent>) {
        match v.get("type").and_then(Value::as_str) {
            Some("user") => self.user(v, out),
            Some("assistant") => self.assistant(v, out),
            Some("attachment") => {
                let att = v.get("attachment");
                if att.and_then(|a| a.get("type")).and_then(Value::as_str) == Some("queued_command") {
                    if let Some(p) = att.and_then(|a| a.get("prompt")).and_then(Value::as_str) {
                        // A completion notice can also arrive mid-turn, as
                        // absorbed input rather than a new prompt (seen
                        // 2026-09-24 when the model polled the output file
                        // and the notice landed during that tool call).
                        self.bg_notice(p, out);
                        out.push(TrackEvent::Absorbed { text: p.to_string() });
                    }
                }
            }
            Some("queue-operation") => {
                if v.get("operation").and_then(Value::as_str) == Some("enqueue") {
                    if let Some(c) = v.get("content").and_then(Value::as_str) {
                        out.push(TrackEvent::Enqueued { text: c.to_string() });
                    }
                }
            }
            Some("system") => {
                let subtype = v.get("subtype").and_then(Value::as_str);
                if subtype == Some("turn_duration") && self.open {
                    self.end_turn(out);
                } else if subtype == Some("compact_boundary") {
                    // The session's context was replaced by a summary. An open
                    // turn stays open and ends on its own turn_duration.
                    let trigger = v
                        .pointer("/compactMetadata/trigger")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string();
                    out.push(TrackEvent::Compacted { trigger });
                }
            }
            _ => {}
        }
    }

    fn user(&mut self, v: &Value, out: &mut Vec<TrackEvent>) {
        // The compaction summary is a user record shown only in the transcript.
        let flag = |k: &str| v.get(k).and_then(Value::as_bool) == Some(true);
        if flag("isMeta") || flag("isCompactSummary") || flag("isVisibleInTranscriptOnly") {
            return;
        }
        let content = v.pointer("/message/content");
        let text = match content {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(items)) => {
                if items
                    .iter()
                    .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                {
                    // Tool results are part of the running turn, not prompts. A
                    // failed result of a background call means the command never
                    // started (the permission classifier or the operator
                    // refused it).
                    for b in items {
                        if b.get("type").and_then(Value::as_str) == Some("tool_result")
                            && b.get("is_error").and_then(Value::as_bool) == Some(true)
                        {
                            if let Some(id) = b.get("tool_use_id").and_then(Value::as_str) {
                                if self.pending_bg.remove(id) {
                                    out.push(TrackEvent::BgNeverStarted { id: id.to_string() });
                                }
                            }
                        }
                    }
                    return;
                }
                items
                    .iter()
                    .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|b| b.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            _ => return,
        };
        if text.trim().is_empty() || text.trim_start().starts_with("[Request interrupted") {
            return;
        }
        if local_command(v, &text) {
            return;
        }
        let origin = v
            .pointer("/origin/kind")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        if origin == "task-notification" {
            self.bg_notice(&text, out);
        }
        let starts_turn = !self.open;
        if starts_turn {
            self.open = true;
            self.last_msg_id = None;
            self.last_msg_text.clear();
        }
        out.push(TrackEvent::Prompt { origin, text, starts_turn });
    }

    /// A `<task-notification>` naming a pending background tool call.
    fn bg_notice(&mut self, text: &str, out: &mut Vec<TrackEvent>) {
        if !text.contains("<task-notification>") {
            return;
        }
        if let Some(id) = between(text, "<tool-use-id>", "</tool-use-id>") {
            if self.pending_bg.remove(id) {
                out.push(TrackEvent::BgFinished { id: id.to_string() });
            }
        }
    }

    fn assistant(&mut self, v: &Value, out: &mut Vec<TrackEvent>) {
        let Some(msg) = v.get("message") else { return };
        let id = msg.get("id").and_then(Value::as_str).unwrap_or("").to_string();
        let stop = msg.get("stop_reason").and_then(Value::as_str).unwrap_or("");
        if self.last_msg_id.as_deref() != Some(id.as_str()) {
            self.last_msg_id = Some(id);
            self.last_msg_text.clear();
        }
        let Some(blocks) = msg.get("content").and_then(Value::as_array) else { return };
        let mut text = String::new();
        for b in blocks {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(t) = b.get("text").and_then(Value::as_str) {
                        text.push_str(t);
                    }
                }
                Some("tool_use") => {
                    let bg = b.pointer("/input/run_in_background").and_then(Value::as_bool)
                        == Some(true);
                    if bg {
                        if let Some(tid) = b.get("id").and_then(Value::as_str) {
                            if self.pending_bg.insert(tid.to_string()) {
                                out.push(TrackEvent::BgStarted {
                                    id: tid.to_string(),
                                    description: bg_description(b.get("input")),
                                });
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        if text.trim().is_empty() {
            return;
        }
        if !self.last_msg_text.is_empty() {
            self.last_msg_text.push('\n');
        }
        self.last_msg_text.push_str(text.trim());
        if stop == "tool_use" && self.open {
            out.push(TrackEvent::Progress { text: text.trim().to_string() });
        }
    }

    fn end_turn(&mut self, out: &mut Vec<TrackEvent>) {
        let final_text =
            (!self.last_msg_text.trim().is_empty()).then(|| self.last_msg_text.trim().to_string());
        self.open = false;
        self.last_msg_id = None;
        self.last_msg_text.clear();
        out.push(TrackEvent::TurnEnd { final_text, pending_bg: self.pending_bg.len() });
    }
}

/// Text of the records a slash command typed into the session leaves
/// (`/compact`, `/model`): the command's name and arguments and a local
/// command's output. A local command writes no turn_duration, so a turn these
/// records opened would never end.
const LOCAL_COMMAND_PREFIXES: &[&str] =
    &["<command-name>", "<command-message>", "<command-args>", "<local-command-"];

/// A slash command line as the harness records it: one line, no `origin`.
fn slash_command_line(t: &str) -> bool {
    let Some(rest) = t.strip_prefix('/') else { return false };
    if t.contains('\n') || !rest.starts_with(|c: char| c.is_ascii_alphabetic()) {
        return false;
    }
    let name_end = rest.find(' ').unwrap_or(rest.len());
    rest[..name_end].chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ':' | '-'))
}

fn local_command(v: &Value, text: &str) -> bool {
    let t = text.trim();
    if LOCAL_COMMAND_PREFIXES.iter().any(|p| t.starts_with(p)) {
        return true;
    }
    v.get("origin").is_none() && slash_command_line(t)
}

/// Longest description kept for a background command, in characters.
const BG_DESCRIPTION_CHARS: usize = 120;

/// What a background tool call says it runs: the first line of its
/// `description`, else the first line of its `command`, cut to
/// [`BG_DESCRIPTION_CHARS`] characters; `None` when it has neither.
fn bg_description(input: Option<&Value>) -> Option<String> {
    let first_line = |k: &str| {
        input
            .and_then(|i| i.get(k))
            .and_then(Value::as_str)
            .map(|s| s.trim().split('\n').next().unwrap_or("").trim().to_string())
            .unwrap_or_default()
    };
    let mut d = first_line("description");
    if d.is_empty() {
        d = first_line("command");
    }
    if d.is_empty() {
        return None;
    }
    if d.chars().count() > BG_DESCRIPTION_CHARS {
        let cut: String = d.chars().take(BG_DESCRIPTION_CHARS).collect();
        return Some(format!("{cut}…"));
    }
    Some(d)
}

/// Turns that ended after the last compaction of a whole transcript (after
/// its start when it was never compacted). The daily compaction skips a
/// session with fewer than its threshold. Pure.
pub fn turns_since_compaction(transcript: &str) -> usize {
    let mut t = TurnTracker::new();
    let mut n = 0usize;
    let mut events = t.feed(transcript);
    if !transcript.ends_with('\n') {
        events.extend(t.feed("\n"));
    }
    for ev in events {
        match ev {
            TrackEvent::Compacted { .. } => n = 0,
            TrackEvent::TurnEnd { .. } => n += 1,
            _ => {}
        }
    }
    n
}

fn between<'a>(s: &'a str, open: &str, close: &str) -> Option<&'a str> {
    let start = s.find(open)? + open.len();
    let len = s[start..].find(close)?;
    Some(s[start..start + len].trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    const VECTORS: &str = include_str!("../testdata/turn_tracker_vectors.json");

    #[test]
    fn shared_vectors() {
        let doc: Value = serde_json::from_str(VECTORS).unwrap();
        let cases = doc["cases"].as_array().unwrap();
        assert!(!cases.is_empty());
        for case in cases {
            let name = case["name"].as_str().unwrap();
            let mut t = TurnTracker::new();
            // Feed as raw JSONL, split at an arbitrary byte boundary, so the
            // partial-line buffering is exercised by every case.
            let jsonl: String = case["records"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| format!("{}\n", serde_json::to_string(r).unwrap()))
                .collect();
            let cut = jsonl.len() / 2;
            let cut = (0..=cut).rev().find(|i| jsonl.is_char_boundary(*i)).unwrap();
            let mut got = t.feed(&jsonl[..cut]);
            got.extend(t.feed(&jsonl[cut..]));
            let want: Vec<TrackEvent> = serde_json::from_value(case["events"].clone()).unwrap();
            assert_eq!(got, want, "case {name:?}");
            if let Some(n) = case.get("turns_since_compaction").and_then(Value::as_u64) {
                assert_eq!(turns_since_compaction(&jsonl), n as usize, "case {name:?}");
            }
        }
    }

    #[test]
    fn partial_line_is_buffered() {
        let mut t = TurnTracker::new();
        let rec = r#"{"type":"user","origin":{"kind":"human"},"message":{"content":"hi"}}"#;
        assert!(t.feed(&rec[..10]).is_empty());
        let ev = t.feed(&format!("{}\n", &rec[10..]));
        assert_eq!(ev.len(), 1);
        assert!(t.turn_open());
    }
}
