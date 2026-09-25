//! Transcript turn tracker (ADR-033).
//!
//! Reads the Claude Code transcript JSONL incrementally and reports what the
//! session is doing in terms a caller can act on: a prompt was accepted, a
//! message typed during a running turn was absorbed, the model wrote
//! intermediate text before a tool call, a background task started or
//! finished, and the turn ended.
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
    BgStarted { id: String },
    /// The harness reported a background task as finished.
    BgFinished { id: String },
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
                if v.get("subtype").and_then(Value::as_str) == Some("turn_duration") && self.open {
                    self.end_turn(out);
                }
            }
            _ => {}
        }
    }

    fn user(&mut self, v: &Value, out: &mut Vec<TrackEvent>) {
        if v.get("isMeta").and_then(Value::as_bool) == Some(true) {
            return;
        }
        let content = v.pointer("/message/content");
        let text = match content {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(items)) => {
                // Tool results are part of the running turn, not prompts.
                if items
                    .iter()
                    .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                {
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
                                out.push(TrackEvent::BgStarted { id: tid.to_string() });
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
