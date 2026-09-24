//! Codex session-log parser (ADR-034).
//!
//! Files: `<sessions>/YYYY/MM/DD/rollout-<ts>-<thread id>.jsonl`, one thread
//! per file. The first `session_meta` line names the thread, its working
//! directory, and — for a subagent thread — the parent thread
//! (`source.subagent.thread_spawn.parent_thread_id`); a subagent's usage is
//! attributed to the parent session. `turn_context` lines carry the model in
//! effect for the following turns.
//!
//! Usage lives on `event_msg` / `token_count` events: `info.total_token_usage`
//! is the thread's cumulative total and `info.last_token_usage` the usage of
//! the response that produced the event. Codex re-emits an unchanged event
//! after some turns (same totals), and a thread can restart its counter
//! (totals drop; `last` then equals `total`). Rule: an event whose
//! cumulative totals equal the previous event's totals in every field
//! (input, cached, cache writes, output, reasoning, total) is a repeat and
//! is skipped; every other event contributes its `last_token_usage`.
//! Comparing all fields, not only `total_tokens`, keeps a counter reset
//! whose new total happens to equal the old one from being taken for a
//! repeat. Summing `last` instead of differencing totals is also correct
//! for a forked thread, whose first total includes the parent's history but
//! whose `last` is its own first response.
//!
//! The row key is built from the event's content — thread id, timestamp and
//! the cumulative totals — never from its byte position, so re-reading a
//! rewritten file produces the same keys.
//!
//! OpenAI counts cached tokens inside `input_tokens` and reasoning tokens
//! inside `output_tokens`. The row stores `input = input - cached` so the
//! token categories do not overlap, and `reasoning` as a display-only subset
//! of `output`.
//!
//! `rate_limits` on the same events is the Codex plan's rate-limit reading
//! (primary window = weekly on current plans); a non-null
//! `rate_limit_reached_type` is a limit hit.

use super::pricing::{Tokens, normalize_model};
use super::records::*;
use memchr::memmem;
use serde::Deserialize;

#[derive(Debug, Clone, Default, serde::Serialize, Deserialize)]
pub struct Carry {
    /// This file's thread id (from `session_meta`).
    pub thread_id: Option<String>,
    /// The session the usage is attributed to: the parent thread for a
    /// subagent, the thread itself otherwise.
    pub session_id: Option<String>,
    pub model: Option<String>,
    /// Cumulative totals of the last counted event.
    pub prev_total: Option<Totals>,
    /// Timestamp of the first parsed line.
    #[serde(default)]
    pub first_ts_ms: Option<i64>,
}

/// Every field of a cumulative `total_token_usage`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, Deserialize)]
pub struct Totals {
    pub input: i64,
    pub cached: i64,
    pub cache_write: i64,
    pub output: i64,
    pub reasoning: i64,
    pub total: i64,
}

#[derive(Deserialize)]
struct Line {
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    payload: Option<Payload>,
}

#[derive(Deserialize)]
struct Payload {
    #[serde(rename = "type")]
    kind: Option<String>,
    // session_meta
    id: Option<String>,
    cwd: Option<String>,
    originator: Option<String>,
    source: Option<serde_json::Value>,
    // turn_context
    model: Option<String>,
    // token_count
    info: Option<Info>,
    rate_limits: Option<RateLimits>,
}

#[derive(Deserialize)]
struct Info {
    total_token_usage: Option<TokenUsage>,
    last_token_usage: Option<TokenUsage>,
}

#[derive(Deserialize, Default, Clone, Copy)]
struct TokenUsage {
    #[serde(default)]
    input_tokens: i64,
    #[serde(default)]
    cached_input_tokens: i64,
    #[serde(default)]
    cache_write_input_tokens: i64,
    #[serde(default)]
    output_tokens: i64,
    #[serde(default)]
    reasoning_output_tokens: i64,
    #[serde(default)]
    total_tokens: i64,
}

#[derive(Deserialize)]
struct RateLimits {
    primary: Option<RateWindow>,
    secondary: Option<RateWindow>,
    plan_type: Option<String>,
    rate_limit_reached_type: Option<String>,
}

#[derive(Deserialize)]
struct RateWindow {
    used_percent: Option<f64>,
    window_minutes: Option<i64>,
    resets_at: Option<i64>,
}

fn wanted(line: &[u8]) -> bool {
    memmem::find(line, b"\"token_count\"").is_some()
        || memmem::find(line, b"\"type\":\"turn_context\"").is_some()
        || memmem::find(line, b"\"type\":\"session_meta\"").is_some()
}

fn parent_thread(source: &serde_json::Value) -> Option<String> {
    source
        .get("subagent")?
        .get("thread_spawn")?
        .get("parent_thread_id")?
        .as_str()
        .map(String::from)
}

/// Parse one line.
pub fn parse_line(line: &[u8], carry: &mut Carry, out: &mut Vec<Record>) -> LineStatus {
    if !wanted(line) {
        return LineStatus::Ignored;
    }
    let Ok(l) = serde_json::from_slice::<Line>(line) else {
        return LineStatus::Malformed;
    };
    let ts = l.timestamp.as_deref().and_then(parse_ts_ms);
    if let Some(t) = ts {
        carry.first_ts_ms.get_or_insert(t);
    }
    let Some(p) = l.payload else { return LineStatus::Parsed };
    let ts_ms = ts.unwrap_or(0);
    parse_payload(l.kind.as_deref(), p, ts_ms, carry, out);
    LineStatus::Parsed
}

fn parse_payload(kind: Option<&str>, p: Payload, ts_ms: i64, carry: &mut Carry, out: &mut Vec<Record>) {

    match (kind, p.kind.as_deref()) {
        (Some("session_meta"), _) => {
            // A forked thread repeats its parent's meta after its own; the
            // first one names this file's thread.
            if carry.thread_id.is_some() {
                return;
            }
            let Some(id) = p.id else { return };
            let parent = p.source.as_ref().and_then(parent_thread);
            carry.session_id = Some(parent.clone().unwrap_or_else(|| id.clone()));
            carry.thread_id = Some(id.clone());
            if parent.is_none() {
                out.push(Record::Session(SessionInfo {
                    session_id: id,
                    cwd: p.cwd,
                    originator: p.originator,
                    ..Default::default()
                }));
            }
        }
        (Some("turn_context"), _) => {
            if let Some(m) = p.model {
                carry.model = Some(normalize_model(&m));
            }
        }
        (Some("event_msg"), Some("token_count")) => {
            let (Some(thread), Some(session)) = (carry.thread_id.clone(), carry.session_id.clone())
            else {
                return;
            };
            if let Some(rl) = &p.rate_limits {
                rate_records(rl, ts_ms, &session, out);
            }
            let Some(info) = p.info else { return };
            let (Some(total), Some(last)) = (info.total_token_usage, info.last_token_usage) else {
                return;
            };
            let totals = Totals {
                input: total.input_tokens,
                cached: total.cached_input_tokens,
                cache_write: total.cache_write_input_tokens,
                output: total.output_tokens,
                reasoning: total.reasoning_output_tokens,
                total: total.total_tokens,
            };
            if carry.prev_total == Some(totals) {
                return; // re-emitted event
            }
            carry.prev_total = Some(totals);
            let cached = last.cached_input_tokens.min(last.input_tokens);
            let t = totals;
            out.push(Record::Usage(UsageRow {
                key: format!(
                    "codex:{thread}:{ts_ms}:{}:{}:{}:{}:{}:{}",
                    t.input, t.cached, t.cache_write, t.output, t.reasoning, t.total
                ),
                vendor: Vendor::Codex,
                session_id: session,
                subagent_id: (carry.session_id.as_deref() != Some(thread.as_str()))
                    .then(|| thread.clone()),
                ts_ms,
                model: carry.model.clone().unwrap_or_else(|| "codex-unknown".to_string()),
                tokens: Tokens {
                    input: last.input_tokens - cached,
                    cache_write_5m: last.cache_write_input_tokens,
                    cache_write_1h: 0,
                    cache_read: cached,
                    output: last.output_tokens,
                },
                reasoning: last.reasoning_output_tokens,
            }));
        }
        _ => {}
    }
}

fn rate_records(rl: &RateLimits, ts_ms: i64, session: &str, out: &mut Vec<Record>) {
    for (slot, w) in [("primary", &rl.primary), ("secondary", &rl.secondary)] {
        let Some(w) = w else { continue };
        let Some(used) = w.used_percent else { continue };
        out.push(Record::Rate(RateSnapshot {
            ts_ms,
            slot: slot.to_string(),
            used_percent: used,
            window_minutes: w.window_minutes,
            resets_at: w.resets_at,
            plan_type: rl.plan_type.clone(),
        }));
    }
    if let Some(kind) = &rl.rate_limit_reached_type {
        let resets = rl.primary.as_ref().and_then(|w| w.resets_at);
        out.push(Record::Limit(LimitEvent {
            // One event per limit type and reset window: Codex repeats the
            // flag on every event until the window resets.
            key: format!("codex-limit:{kind}:{}", resets.unwrap_or(ts_ms / 1000)),
            vendor: Vendor::Codex,
            session_id: session.to_string(),
            ts_ms,
            kind: "rate_limit".to_string(),
            status: None,
            limit_type: Some(kind.clone()),
            resets_at: resets,
            message: None,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(lines: &[&str]) -> (Vec<Record>, Carry) {
        let mut carry = Carry::default();
        let mut out = Vec::new();
        for l in lines {
            parse_line(l.as_bytes(), &mut carry, &mut out);
        }
        (out, carry)
    }

    fn rows(r: &[Record]) -> Vec<&UsageRow> {
        r.iter().filter_map(|r| if let Record::Usage(u) = r { Some(u) } else { None }).collect()
    }

    fn tc(total: (i64, i64, i64, i64), last: (i64, i64, i64, i64)) -> String {
        tc_at("2026-09-01T12:00:00Z", total, last)
    }

    fn tc_at(ts: &str, total: (i64, i64, i64, i64), last: (i64, i64, i64, i64)) -> String {
        let u = |(i, c, o, r): (i64, i64, i64, i64)| {
            format!(
                r#"{{"input_tokens":{i},"cached_input_tokens":{c},"cache_write_input_tokens":0,"output_tokens":{o},"reasoning_output_tokens":{r},"total_tokens":{}}}"#,
                i + o
            )
        };
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{},"last_token_usage":{}}},"rate_limits":{{"primary":{{"used_percent":12.0,"window_minutes":10080,"resets_at":1790000000}},"secondary":null,"plan_type":"plus","rate_limit_reached_type":null}}}}}}"#,
            u(total),
            u(last)
        )
    }

    const META: &str = r#"{"timestamp":"2026-09-01T11:59:00Z","type":"session_meta","payload":{"id":"t-1","cwd":"/work/repo","originator":"codex-tui","source":"cli"}}"#;
    const CTX: &str = r#"{"timestamp":"2026-09-01T11:59:01Z","type":"turn_context","payload":{"model":"gpt-5.6-sol","cwd":"/work/repo"}}"#;

    #[test]
    fn cumulative_totals_repeats_and_resets() {
        let (r, carry) = run(&[
            META,
            CTX,
            &tc((1000, 800, 50, 10), (1000, 800, 50, 10)),
            &tc((1000, 800, 50, 10), (1000, 800, 50, 10)), // repeat: skipped
            &tc((2600, 2200, 90, 20), (1600, 1400, 40, 10)),
            &tc((300, 0, 5, 0), (300, 0, 5, 0)), // counter reset: last == total
        ]);
        let rows = rows(&r);
        assert_eq!(rows.len(), 3);
        let input: i64 = rows.iter().map(|r| r.tokens.input).sum();
        let cached: i64 = rows.iter().map(|r| r.tokens.cache_read).sum();
        let out: i64 = rows.iter().map(|r| r.tokens.output).sum();
        assert_eq!(cached, 800 + 1400);
        assert_eq!(input, (1000 - 800) + (1600 - 1400) + 300, "input excludes cached");
        assert_eq!(out, 50 + 40 + 5);
        assert_eq!(rows[0].model, "gpt-5.6-sol");
        assert_eq!(rows[0].session_id, "t-1");
        assert!(rows[0].subagent_id.is_none());
        assert_eq!(carry.prev_total.map(|t| t.total), Some(305));
        // one rate snapshot per event, including the repeat
        assert_eq!(r.iter().filter(|r| matches!(r, Record::Rate(_))).count(), 4);
    }

    #[test]
    fn counter_reset_with_a_colliding_total_is_not_a_repeat() {
        // The reset event's total_tokens (1050) equals the previous event's,
        // but its fields differ: it is new usage, not a re-emitted event.
        let (r, _) = run(&[
            META,
            CTX,
            &tc_at("2026-09-01T12:00:00Z", (1000, 800, 50, 0), (1000, 800, 50, 0)),
            &tc_at("2026-09-01T12:10:00Z", (1040, 0, 10, 0), (1040, 0, 10, 0)),
        ]);
        let rows = rows(&r);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].tokens.input, 1040);
        assert_eq!(rows[1].tokens.output, 10);
    }

    #[test]
    fn keys_depend_on_content_not_position() {
        let ev = tc((1000, 800, 50, 10), (1000, 800, 50, 10));
        let (a, _) = run(&[META, CTX, &ev]);
        // The same event behind extra lines: same key.
        let noise = r#"{"timestamp":"2026-09-01T11:59:30Z","type":"response_item","payload":{"type":"message"}}"#;
        let (b, _) = run(&[META, noise, noise, CTX, &ev]);
        assert_eq!(rows(&a)[0].key, rows(&b)[0].key);
        assert!(rows(&a)[0].key.starts_with("codex:t-1:"));
    }

    #[test]
    fn malformed_relevant_lines_are_reported() {
        let mut carry = Carry::default();
        let mut out = Vec::new();
        let bad = br#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":"#;
        assert_eq!(parse_line(bad, &mut carry, &mut out), LineStatus::Malformed);
        assert_eq!(parse_line(b"not json at all", &mut carry, &mut out), LineStatus::Ignored);
    }

    #[test]
    fn forked_subagent_counts_last_not_inherited_total() {
        let meta = r#"{"timestamp":"2026-09-01T11:59:00Z","type":"session_meta","payload":{"id":"t-child","cwd":"/work/repo","originator":"codex-tui","source":{"subagent":{"thread_spawn":{"parent_thread_id":"t-parent","depth":1}}}}}"#;
        let (r, _) = run(&[meta, CTX, &tc((500_000, 490_000, 900, 0), (1200, 1000, 30, 0))]);
        let rows = rows(&r);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].session_id, "t-parent");
        assert_eq!(rows[0].subagent_id.as_deref(), Some("t-child"));
        assert_eq!(rows[0].tokens.cache_read, 1000);
        assert_eq!(rows[0].tokens.input, 200);
        assert!(!r.iter().any(|r| matches!(r, Record::Session(_))), "child is not a session");
    }

    #[test]
    fn limit_reached_collapses_per_window() {
        let hit = r#"{"timestamp":"2026-09-01T12:00:00Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"primary":{"used_percent":100.0,"window_minutes":10080,"resets_at":1790000000},"plan_type":"plus","rate_limit_reached_type":"primary"}}}"#;
        let (r, _) = run(&[META, hit, hit]);
        let keys: std::collections::HashSet<_> = r
            .iter()
            .filter_map(|r| if let Record::Limit(e) = r { Some(e.key.clone()) } else { None })
            .collect();
        assert_eq!(keys.len(), 1);
    }
}
