//! Claude Code transcript parser (ADR-034).
//!
//! Files: `<projects>/<encoded-cwd>/<session>.jsonl` (main) and
//! `<projects>/<encoded-cwd>/<session>/subagents/agent-<id>.jsonl` (one per
//! subagent; not included in the main file, attributed to the parent).
//!
//! Usage lives on `type:"assistant"` lines as `message.usage`. One API
//! response is written as several lines — one per content block, each
//! repeating the response's usage — and a streamed response also writes
//! partial lines (`output_tokens` 1, `stop_reason` null) before the final
//! one. The dedupe key is `message.id` (fallback `requestId`, then the line
//! `uuid`); among lines with the same key the one with the largest
//! `output_tokens` wins. The store applies the same rule across refreshes
//! (upsert only when the new output is not smaller), so a response split
//! across two refreshes still resolves to its final line.
//!
//! Synthetic assistant lines (`isApiErrorMessage`, model `<synthetic>`)
//! carry API errors — usage limits (429 + `quotaLimits`), overload (529),
//! "prompt is too long", auth failures — and become [`LimitEvent`]s.
//!
//! `type:"cost-state"` lines are Claude Code's own running totals for one
//! process run; see `reconcile.rs` for how they combine with the responses.

use super::pricing::{Tokens, normalize_model};
use super::records::*;
use memchr::memmem;
use serde::Deserialize;

/// Where a file's records are attributed.
#[derive(Debug, Clone)]
pub struct FileCtx {
    /// The top-level session id (the main file's stem, or the parent
    /// session directory for a subagent file).
    pub session_id: String,
    pub subagent_id: Option<String>,
}

/// State carried across lines, and across refreshes for incremental reads.
#[derive(Debug, Clone, Default, serde::Serialize, Deserialize)]
pub struct Carry {
    /// Latest line timestamp seen so far in this file.
    pub last_ts_ms: Option<i64>,
    /// Whether the session's working directory was already emitted.
    pub cwd_seen: bool,
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "requestId")]
    request_id: Option<String>,
    uuid: Option<String>,
    #[serde(rename = "isApiErrorMessage", default)]
    is_api_error: bool,
    error: Option<String>,
    #[serde(rename = "apiErrorStatus")]
    api_error_status: Option<i64>,
    #[serde(rename = "quotaLimits")]
    quota_limits: Option<QuotaLimits>,
    message: Option<Message>,
    // cost-state
    #[serde(rename = "startTime")]
    start_time: Option<i64>,
    #[serde(rename = "modelUsage")]
    model_usage: Option<std::collections::BTreeMap<String, ModelUsage>>,
    // titles
    #[serde(rename = "aiTitle")]
    ai_title: Option<String>,
    #[serde(rename = "customTitle")]
    custom_title: Option<String>,
}

#[derive(Deserialize)]
struct QuotaLimits {
    #[serde(rename = "rateLimitType")]
    rate_limit_type: Option<String>,
    #[serde(rename = "resetsAt")]
    resets_at: Option<i64>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize, Default)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<i64>,
    #[serde(default)]
    output_tokens: Option<i64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<i64>,
    #[serde(default)]
    cache_read_input_tokens: Option<i64>,
    cache_creation: Option<CacheCreation>,
    output_tokens_details: Option<OutputDetails>,
}

#[derive(Deserialize)]
struct CacheCreation {
    ephemeral_1h_input_tokens: Option<i64>,
    ephemeral_5m_input_tokens: Option<i64>,
}

#[derive(Deserialize)]
struct OutputDetails {
    thinking_tokens: Option<i64>,
}

#[derive(Deserialize)]
struct ModelUsage {
    #[serde(rename = "inputTokens", default)]
    input: i64,
    #[serde(rename = "outputTokens", default)]
    output: i64,
    #[serde(rename = "cacheReadInputTokens", default)]
    cache_read: i64,
    #[serde(rename = "cacheCreationInputTokens", default)]
    cache_write: i64,
    #[serde(rename = "webSearchRequests", default)]
    web_search: i64,
    #[serde(rename = "costUSD", default)]
    cost_usd: f64,
}

/// Only the error text of a synthetic line; parsed on error lines only.
#[derive(Deserialize)]
struct ErrorLine {
    message: Option<ErrorMessage>,
}

#[derive(Deserialize)]
struct ErrorMessage {
    content: Option<Vec<ErrorBlock>>,
}

#[derive(Deserialize)]
struct ErrorBlock {
    text: Option<String>,
}

const TS_KEY: &[u8] = b"\"timestamp\":\"";

/// Read a top-level-looking `"timestamp":"…"` without parsing the line. Used
/// on lines the parser otherwise skips, so cost-state snapshots get a window
/// end even when the preceding lines are not assistant lines.
fn quick_timestamp(line: &[u8]) -> Option<i64> {
    let at = memmem::find(line, TS_KEY)? + TS_KEY.len();
    let rest = &line[at..];
    let end = memchr::memchr(b'"', rest)?;
    std::str::from_utf8(&rest[..end]).ok().and_then(parse_ts_ms)
}

fn wanted(line: &[u8], carry: &Carry) -> bool {
    memmem::find(line, b"\"type\":\"assistant\"").is_some()
        || memmem::find(line, b"\"type\":\"cost-state\"").is_some()
        || memmem::find(line, b"\"type\":\"ai-title\"").is_some()
        || memmem::find(line, b"\"type\":\"custom-title\"").is_some()
        || (!carry.cwd_seen && memmem::find(line, b"\"cwd\":\"").is_some())
}

/// Parse one line. Lines that fail to parse are skipped (a half-written
/// final line never reaches here: the reader stops at the last newline).
pub fn parse_line(line: &[u8], ctx: &FileCtx, carry: &mut Carry, out: &mut Vec<Record>) {
    if let Some(ts) = quick_timestamp(line) {
        carry.last_ts_ms = Some(carry.last_ts_ms.map_or(ts, |t| t.max(ts)));
    }
    if !wanted(line, carry) {
        return;
    }
    let Ok(l) = serde_json::from_slice::<Line>(line) else {
        return;
    };

    if !carry.cwd_seen && ctx.subagent_id.is_none() {
        if let Some(cwd) = l.cwd.as_deref().filter(|c| !c.is_empty()) {
            carry.cwd_seen = true;
            out.push(Record::Session(SessionInfo {
                session_id: ctx.session_id.clone(),
                cwd: Some(cwd.to_string()),
                ..Default::default()
            }));
        }
    }

    let ts_ms = l.timestamp.as_deref().and_then(parse_ts_ms);
    match l.kind.as_deref() {
        Some("assistant") => assistant(line, l, ts_ms, ctx, carry, out),
        Some("cost-state") => {
            let (Some(start_ms), Some(usage)) = (l.start_time, l.model_usage) else {
                return;
            };
            let models: Vec<CostRunModel> = usage
                .into_iter()
                .map(|(model, u)| CostRunModel {
                    model: normalize_model(&model),
                    input: u.input,
                    output: u.output,
                    cache_read: u.cache_read,
                    cache_write: u.cache_write,
                    web_search_requests: u.web_search,
                    cost_usd: u.cost_usd,
                })
                .collect();
            if models.is_empty() {
                return;
            }
            // cost-state lives only in main files; a subagent never writes one.
            out.push(Record::CostRun(CostRun {
                session_id: ctx.session_id.clone(),
                start_ms,
                snapshot_ts_ms: carry.last_ts_ms,
                models,
            }));
        }
        Some("ai-title") | Some("custom-title") if ctx.subagent_id.is_none() => {
            out.push(Record::Session(SessionInfo {
                session_id: ctx.session_id.clone(),
                ai_title: l.ai_title.map(|t| truncate(&t, 160)),
                custom_title: l.custom_title.map(|t| truncate(&t, 160)),
                ..Default::default()
            }));
        }
        _ => {}
    }
}

fn assistant(
    raw: &[u8],
    l: Line,
    ts_ms: Option<i64>,
    ctx: &FileCtx,
    carry: &Carry,
    out: &mut Vec<Record>,
) {
    let ts_ms = ts_ms.or(carry.last_ts_ms).unwrap_or(0);
    let msg = l.message;
    let model = msg.as_ref().and_then(|m| m.model.clone()).unwrap_or_default();

    if l.is_api_error || model == "<synthetic>" {
        let text = serde_json::from_slice::<ErrorLine>(raw)
            .ok()
            .and_then(|e| e.message)
            .and_then(|m| m.content)
            .and_then(|c| c.into_iter().find_map(|b| b.text))
            .map(|t| truncate(&t, 300));
        let key_id = l
            .uuid
            .clone()
            .or_else(|| msg.as_ref().and_then(|m| m.id.clone()))
            .unwrap_or_else(|| format!("{}:{ts_ms}", ctx.session_id));
        out.push(Record::Limit(LimitEvent {
            key: format!("claude-err:{key_id}"),
            vendor: Vendor::Claude,
            session_id: ctx.session_id.clone(),
            ts_ms,
            kind: l.error.clone().unwrap_or_else(|| "api_error".to_string()),
            status: l.api_error_status,
            limit_type: l.quota_limits.as_ref().and_then(|q| q.rate_limit_type.clone()),
            resets_at: l.quota_limits.as_ref().and_then(|q| q.resets_at),
            message: text,
        }));
        return;
    }

    let Some(msg) = msg else { return };
    let Some(u) = msg.usage else { return };
    let Some(id) = msg.id.or(l.request_id).or(l.uuid) else {
        return;
    };

    let cache_write_total = u.cache_creation_input_tokens.unwrap_or(0);
    let (mut w5m, mut w1h) = match &u.cache_creation {
        Some(c) => (
            c.ephemeral_5m_input_tokens.unwrap_or(0),
            c.ephemeral_1h_input_tokens.unwrap_or(0),
        ),
        None => (0, 0),
    };
    if w5m + w1h == 0 {
        // No duration split recorded: Anthropic's default cache duration.
        w5m = cache_write_total;
    } else if w5m + w1h < cache_write_total {
        w5m += cache_write_total - (w5m + w1h);
    }
    if w5m + w1h > cache_write_total && cache_write_total > 0 {
        // Split larger than the total: trust the total, keep the ratio.
        let scale = cache_write_total as f64 / (w5m + w1h) as f64;
        w1h = (w1h as f64 * scale).round() as i64;
        w5m = cache_write_total - w1h;
    }

    out.push(Record::Usage(UsageRow {
        key: format!("claude:{id}"),
        vendor: Vendor::Claude,
        session_id: ctx.session_id.clone(),
        subagent_id: ctx.subagent_id.clone(),
        ts_ms,
        model: normalize_model(&model),
        tokens: Tokens {
            input: u.input_tokens.unwrap_or(0),
            cache_write_5m: w5m,
            cache_write_1h: w1h,
            cache_read: u.cache_read_input_tokens.unwrap_or(0),
            output: u.output_tokens.unwrap_or(0),
        },
        reasoning: u
            .output_tokens_details
            .and_then(|d| d.thinking_tokens)
            .unwrap_or(0),
    }));
}

/// Collapse duplicate usage keys within one batch: keep the row with the
/// largest output (ties: the later line). The store applies the same rule
/// against rows already persisted.
pub fn dedupe_batch(records: Vec<Record>) -> Vec<Record> {
    let mut index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut out: Vec<Record> = Vec::with_capacity(records.len());
    for r in records {
        if let Record::Usage(row) = &r {
            if let Some(&i) = index.get(&row.key) {
                if let Record::Usage(prev) = &out[i] {
                    if row.tokens.output >= prev.tokens.output {
                        out[i] = r;
                    }
                }
                continue;
            }
            index.insert(row.key.clone(), out.len());
        }
        out.push(r);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> FileCtx {
        FileCtx { session_id: "s-main".into(), subagent_id: None }
    }

    fn parse_all(lines: &[&str], ctx: &FileCtx) -> Vec<Record> {
        let mut carry = Carry::default();
        let mut out = Vec::new();
        for l in lines {
            parse_line(l.as_bytes(), ctx, &mut carry, &mut out);
        }
        dedupe_batch(out)
    }

    fn usage_rows(r: &[Record]) -> Vec<&UsageRow> {
        r.iter().filter_map(|r| if let Record::Usage(u) = r { Some(u) } else { None }).collect()
    }

    const PARTIAL: &str = r#"{"type":"assistant","timestamp":"2026-09-01T10:00:00.000Z","cwd":"/work/repo","requestId":"req_1","message":{"id":"msg_1","model":"claude-opus-5","usage":{"input_tokens":5,"cache_creation_input_tokens":100,"cache_read_input_tokens":2000,"output_tokens":1,"cache_creation":{"ephemeral_1h_input_tokens":100,"ephemeral_5m_input_tokens":0}},"stop_reason":null,"content":[{"type":"thinking","thinking":""}]}}"#;
    const BLOCK_A: &str = r#"{"type":"assistant","timestamp":"2026-09-01T10:00:05.000Z","requestId":"req_1","message":{"id":"msg_1","model":"claude-opus-5","usage":{"input_tokens":5,"cache_creation_input_tokens":100,"cache_read_input_tokens":2000,"output_tokens":420,"output_tokens_details":{"thinking_tokens":300},"cache_creation":{"ephemeral_1h_input_tokens":100,"ephemeral_5m_input_tokens":0}},"content":[{"type":"text","text":"a"}]}}"#;
    const BLOCK_B: &str = r#"{"type":"assistant","timestamp":"2026-09-01T10:00:05.100Z","requestId":"req_1","message":{"id":"msg_1","model":"claude-opus-5","usage":{"input_tokens":5,"cache_creation_input_tokens":100,"cache_read_input_tokens":2000,"output_tokens":420,"output_tokens_details":{"thinking_tokens":300},"cache_creation":{"ephemeral_1h_input_tokens":100,"ephemeral_5m_input_tokens":0}},"content":[{"type":"tool_use","name":"Bash"}]}}"#;

    #[test]
    fn multi_block_and_streamed_partial_collapse_to_one_response() {
        let r = parse_all(&[PARTIAL, BLOCK_A, BLOCK_B], &ctx());
        let rows = usage_rows(&r);
        assert_eq!(rows.len(), 1, "one response despite three lines");
        let row = rows[0];
        assert_eq!(row.key, "claude:msg_1");
        assert_eq!(row.tokens.output, 420, "final output wins over the streamed partial");
        assert_eq!(row.reasoning, 300);
        assert_eq!(row.tokens.cache_write_1h, 100);
        assert_eq!(row.tokens.cache_write_5m, 0);
        assert_eq!(row.tokens.cache_read, 2000);
        // cwd emitted once, from the first line
        let cwds: Vec<_> = r
            .iter()
            .filter_map(|r| if let Record::Session(s) = r { s.cwd.clone() } else { None })
            .collect();
        assert_eq!(cwds, vec!["/work/repo".to_string()]);
    }

    #[test]
    fn partial_after_final_does_not_shrink_output() {
        let r = parse_all(&[BLOCK_A, PARTIAL], &ctx());
        assert_eq!(usage_rows(&r)[0].tokens.output, 420);
    }

    #[test]
    fn missing_duration_split_counts_as_5m() {
        let line = r#"{"type":"assistant","timestamp":"2026-09-01T10:00:00Z","message":{"id":"m2","model":"claude-haiku-4-5-20251001","usage":{"input_tokens":1,"cache_creation_input_tokens":50,"output_tokens":2}}}"#;
        let r = parse_all(&[line], &ctx());
        let row = usage_rows(&r)[0];
        assert_eq!((row.tokens.cache_write_5m, row.tokens.cache_write_1h), (50, 0));
        assert_eq!(row.model, "claude-haiku-4-5-20251001");
    }

    #[test]
    fn subagent_lines_attribute_to_parent_and_skip_cwd() {
        let sub = FileCtx { session_id: "s-parent".into(), subagent_id: Some("a1".into()) };
        let r = parse_all(&[PARTIAL, BLOCK_A], &sub);
        let rows = usage_rows(&r);
        assert_eq!(rows[0].session_id, "s-parent");
        assert_eq!(rows[0].subagent_id.as_deref(), Some("a1"));
        assert!(!r.iter().any(|r| matches!(r, Record::Session(_))));
    }

    #[test]
    fn usage_limit_and_other_api_errors_become_limit_events() {
        let limit = r#"{"type":"assistant","uuid":"u-429","timestamp":"2026-09-19T23:00:51.675Z","message":{"id":"x","model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0},"content":[{"type":"text","text":"You've hit your session limit"}]},"quotaLimits":{"status":"rejected","resetsAt":1789861800,"rateLimitType":"five_hour"},"error":"rate_limit","isApiErrorMessage":true,"apiErrorStatus":429}"#;
        let overload = r#"{"type":"assistant","uuid":"u-529","timestamp":"2026-09-19T23:10:00Z","message":{"id":"y","model":"<synthetic>","usage":{"input_tokens":0,"output_tokens":0},"content":[{"type":"text","text":"Overloaded"}]},"error":"server_error","isApiErrorMessage":true,"apiErrorStatus":529}"#;
        let r = parse_all(&[limit, overload], &ctx());
        assert!(usage_rows(&r).is_empty(), "synthetic lines carry no usage");
        let ev: Vec<&LimitEvent> =
            r.iter().filter_map(|r| if let Record::Limit(e) = r { Some(e) } else { None }).collect();
        assert_eq!(ev.len(), 2);
        assert_eq!(ev[0].kind, "rate_limit");
        assert_eq!(ev[0].status, Some(429));
        assert_eq!(ev[0].limit_type.as_deref(), Some("five_hour"));
        assert_eq!(ev[0].resets_at, Some(1789861800));
        assert_eq!(ev[0].message.as_deref(), Some("You've hit your session limit"));
        assert_eq!(ev[1].status, Some(529));
    }

    #[test]
    fn cost_state_takes_the_preceding_timestamp_and_normalizes_models() {
        let cost = r#"{"type":"cost-state","sessionId":"s-main","totalCostUSD":1.5,"startTime":1788000000000,"modelUsage":{"claude-opus-5[1m]":{"inputTokens":10,"outputTokens":20,"cacheReadInputTokens":30,"cacheCreationInputTokens":40,"webSearchRequests":0,"costUSD":1.4},"claude-haiku-4-5-20251001":{"inputTokens":500,"outputTokens":12,"cacheReadInputTokens":0,"cacheCreationInputTokens":0,"costUSD":0.1}}}"#;
        let r = parse_all(&[BLOCK_A, cost], &ctx());
        let run = r
            .iter()
            .find_map(|r| if let Record::CostRun(c) = r { Some(c) } else { None })
            .unwrap();
        assert_eq!(run.start_ms, 1788000000000);
        assert_eq!(run.snapshot_ts_ms, parse_ts_ms("2026-09-01T10:00:05.000Z"));
        let names: Vec<_> = run.models.iter().map(|m| m.model.as_str()).collect();
        assert!(names.contains(&"claude-opus-5"));
        assert!(names.contains(&"claude-haiku-4-5-20251001"));
    }
}
