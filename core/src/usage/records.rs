//! Records the transcript parsers emit and the store writes. Parsers are pure
//! (bytes in, records out) so every dedupe and attribution rule is testable
//! with synthetic lines.

use super::pricing::Tokens;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Claude,
    Codex,
}

impl Vendor {
    pub fn as_str(self) -> &'static str {
        match self {
            Vendor::Claude => "claude",
            Vendor::Codex => "codex",
        }
    }
}

/// One billed model response (Claude) or one turn delta (Codex).
#[derive(Debug, Clone, PartialEq)]
pub struct UsageRow {
    /// Global dedupe key, derived from the line content only (never from
    /// its position in the file). Claude: `claude:<message.id>` (fallback
    /// requestId, then line uuid). Codex: `codex:<thread id>:<event
    /// timestamp>:<cumulative totals>`.
    pub key: String,
    pub vendor: Vendor,
    /// The top-level session the usage is attributed to. A subagent's usage
    /// carries its parent's session id.
    pub session_id: String,
    pub subagent_id: Option<String>,
    pub ts_ms: i64,
    /// Normalized model id (see [`super::pricing::normalize_model`]).
    pub model: String,
    pub tokens: Tokens,
    /// Reasoning/thinking tokens. A subset of `tokens.output`, kept for
    /// display; never priced separately.
    pub reasoning: i64,
}

/// One Claude Code `cost-state` snapshot: the running totals of one process
/// run (identified by `start_ms`) of one session, per model.
#[derive(Debug, Clone, PartialEq)]
pub struct CostRun {
    pub session_id: String,
    pub start_ms: i64,
    /// Latest transcript timestamp at or before the snapshot line: the end
    /// of the window the totals cover.
    pub snapshot_ts_ms: Option<i64>,
    pub models: Vec<CostRunModel>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CostRunModel {
    pub model: String,
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    /// Claude Code does not split cache writes by duration in cost-state.
    pub cache_write: i64,
    pub web_search_requests: i64,
    pub cost_usd: f64,
}

/// An API error or usage-limit event.
#[derive(Debug, Clone, PartialEq)]
pub struct LimitEvent {
    pub key: String,
    pub vendor: Vendor,
    pub session_id: String,
    pub ts_ms: i64,
    /// `rate_limit`, `server_error`, `invalid_request`, `authentication_failed`, …
    pub kind: String,
    pub status: Option<i64>,
    /// `five_hour`, `seven_day`, … (Claude `quotaLimits.rateLimitType`, Codex
    /// `rate_limit_reached_type`).
    pub limit_type: Option<String>,
    /// Unix seconds.
    pub resets_at: Option<i64>,
    pub message: Option<String>,
}

/// One Codex rate-limit reading (`rate_limits.primary` / `.secondary`).
#[derive(Debug, Clone, PartialEq)]
pub struct RateSnapshot {
    pub ts_ms: i64,
    pub slot: String,
    pub used_percent: f64,
    pub window_minutes: Option<i64>,
    /// Unix seconds.
    pub resets_at: Option<i64>,
    pub plan_type: Option<String>,
}

/// Session metadata found in a transcript.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionInfo {
    pub session_id: String,
    pub cwd: Option<String>,
    pub ai_title: Option<String>,
    pub custom_title: Option<String>,
    pub originator: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Record {
    Usage(UsageRow),
    CostRun(CostRun),
    Limit(LimitEvent),
    Session(SessionInfo),
    Rate(RateSnapshot),
}

/// What the parser made of one complete line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineStatus {
    /// Not a line type the parser reads.
    Ignored,
    Parsed,
    /// A line of a type the parser reads that is not valid JSON. Counted and
    /// reported by the refresh; the line itself is skipped.
    Malformed,
}

/// Parse an RFC3339 timestamp to Unix milliseconds.
pub fn parse_ts_ms(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(|d| d.timestamp_millis())
}

pub fn truncate(s: &str, max_chars: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max_chars {
        return t.to_string();
    }
    let mut out: String = t.chars().take(max_chars).collect();
    out.push('…');
    out
}
