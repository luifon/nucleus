//! Read-side aggregates for the dashboard's usage surface (ADR-034). Every
//! function takes a read-only pool; nothing here writes.
//!
//! Token fields: `input` is uncached input, `cache_write` both cache-write
//! durations, `cache_read` cached input, `output` includes reasoning;
//! `tokens` is their sum. `cost_usd` is the dollar estimate (response and
//! residual rows at table prices plus the cost-state adjustments, see
//! `reconcile.rs`); `third_party_usd` is the part of it priced from a
//! third-party estimate rather than a vendor list price.

use anyhow::Result;
use serde::Serialize;
use sqlx::{FromRow, SqlitePool};
use ts_rs::TS;

#[derive(Debug, Clone, Default, Serialize, TS, FromRow)]
#[ts(export)]
pub struct UsageTotals {
    #[ts(type = "number")]
    pub input: i64,
    #[ts(type = "number")]
    pub cache_write: i64,
    #[ts(type = "number")]
    pub cache_read: i64,
    #[ts(type = "number")]
    pub output: i64,
    #[ts(type = "number")]
    pub reasoning: i64,
    #[ts(type = "number")]
    pub tokens: i64,
    pub cost_usd: f64,
    /// Tokens of models the price table does not cover (cost counted as 0
    /// unless a cost-state adjustment priced them).
    #[ts(type = "number")]
    pub unpriced_tokens: i64,
    /// Dollars in `cost_usd` priced from a third-party estimate (a model
    /// whose vendor publishes no price; see the price listing).
    pub third_party_usd: f64,
    /// Number of API responses (Claude) / turn deltas (Codex).
    #[ts(type = "number")]
    pub responses: i64,
}

/// SELECT list producing [`UsageTotals`] over `usage_rows u LEFT JOIN
/// prices p ON p.model = u.model`.
const TOTALS: &str = "
    COALESCE(SUM(u.input),0) AS input,
    COALESCE(SUM(u.cache_write_5m + u.cache_write_1h),0) AS cache_write,
    COALESCE(SUM(u.cache_read),0) AS cache_read,
    COALESCE(SUM(u.output),0) AS output,
    COALESCE(SUM(u.reasoning),0) AS reasoning,
    COALESCE(SUM(u.input + u.cache_write_5m + u.cache_write_1h + u.cache_read + u.output),0) AS tokens,
    COALESCE(SUM(u.cost_usd),0.0) AS cost_usd,
    COALESCE(SUM(CASE WHEN p.matched_key IS NULL AND u.kind != 'adjustment'
                      THEN u.input + u.cache_write_5m + u.cache_write_1h + u.cache_read + u.output ELSE 0 END),0) AS unpriced_tokens,
    COALESCE(SUM(CASE WHEN p.basis = 'third-party-estimate' THEN u.cost_usd ELSE 0.0 END),0.0) AS third_party_usd,
    COALESCE(SUM(CASE WHEN u.kind = 'response' THEN 1 ELSE 0 END),0) AS responses";

const FROM: &str = "FROM usage_rows u LEFT JOIN prices p ON p.model = u.model";

/// Unpriced tokens over `usage_rows u LEFT JOIN prices p`.
const UNPRICED: &str = "COALESCE(SUM(CASE WHEN p.matched_key IS NULL AND u.kind != 'adjustment'
    THEN u.input + u.cache_write_5m + u.cache_write_1h + u.cache_read + u.output ELSE 0 END),0)";
/// Dollars priced from a third-party estimate over the same join.
const THIRD_PARTY: &str = "COALESCE(SUM(CASE WHEN p.basis = 'third-party-estimate' THEN u.cost_usd ELSE 0.0 END),0.0)";

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct UsageCompare {
    pub label: String,
    pub current: UsageTotals,
    pub previous: UsageTotals,
    /// Inclusive local-day bounds of both periods.
    pub current_from: String,
    pub current_to: String,
    pub previous_from: String,
    pub previous_to: String,
}

#[derive(Debug, Clone, Serialize, TS, FromRow)]
#[ts(export)]
pub struct UsageSeriesPoint {
    /// Local day (`YYYY-MM-DD`), or the Monday of the week for weekly series.
    pub bucket: String,
    pub vendor: String,
    #[ts(type = "number")]
    pub tokens: i64,
    #[ts(type = "number")]
    pub cache_read: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Serialize, TS, FromRow)]
#[ts(export)]
pub struct UsageModelRow {
    pub vendor: String,
    pub model: String,
    #[sqlx(flatten)]
    pub totals: UsageTotals,
    #[ts(type = "number")]
    pub sessions: i64,
}

#[derive(Debug, Clone, Serialize, TS, FromRow)]
#[ts(export)]
pub struct UsageHeatCell {
    /// 0 = Monday … 6 = Sunday.
    #[ts(type = "number")]
    pub dow: i64,
    #[ts(type = "number")]
    pub hour: i64,
    #[ts(type = "number")]
    pub tokens: i64,
    pub cost_usd: f64,
}

#[derive(Debug, Clone, Serialize, TS, FromRow)]
#[ts(export)]
pub struct UsageRateReading {
    pub slot: String,
    pub used_percent: f64,
    #[ts(type = "number | null")]
    pub window_minutes: Option<i64>,
    /// RFC3339 UTC.
    pub resets_at: Option<String>,
    pub read_at: String,
    pub plan_type: Option<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct UsageSummary {
    pub days: u32,
    /// Today in the operator's timezone (`NUCLEUS_TZ`).
    pub today: String,
    /// First local day of the range: `today − (days − 1)`, or for all time
    /// the first day with data under the tool filter (today when none).
    pub range_from: String,
    pub by_vendor: Vec<UsageVendorTotals>,
    pub day: UsageCompare,
    pub week: UsageCompare,
    pub range: UsageCompare,
    pub daily: Vec<UsageSeriesPoint>,
    pub weekly: Vec<UsageSeriesPoint>,
    pub models: Vec<UsageModelRow>,
    pub heatmap: Vec<UsageHeatCell>,
    /// Latest Codex rate-limit readings (one per slot), as recorded in the
    /// Codex logs. No equivalent exists for Claude.
    pub codex_quota: Vec<UsageRateReading>,
}

#[derive(Debug, Clone, Serialize, TS, FromRow)]
#[ts(export)]
pub struct UsageVendorTotals {
    pub vendor: String,
    #[sqlx(flatten)]
    pub totals: UsageTotals,
    #[ts(type = "number")]
    pub sessions: i64,
}

#[derive(Debug, Clone, Serialize, TS, FromRow)]
#[ts(export)]
pub struct UsageProjectRow {
    pub project: String,
    pub root: Option<String>,
    #[sqlx(flatten)]
    pub totals: UsageTotals,
    pub claude_cost_usd: f64,
    pub codex_cost_usd: f64,
    #[ts(type = "number")]
    pub claude_tokens: i64,
    #[ts(type = "number")]
    pub codex_tokens: i64,
    #[ts(type = "number")]
    pub sessions: i64,
    pub last_day: Option<String>,
}

#[derive(Debug, Clone, Serialize, TS, FromRow)]
#[ts(export)]
pub struct UsageAgentRow {
    pub agent: String,
    #[sqlx(flatten)]
    pub totals: UsageTotals,
    #[ts(type = "number")]
    pub sessions: i64,
    pub cost_30d: f64,
    /// Tokens in the last 30 days without a price (not in `cost_30d`).
    #[ts(type = "number")]
    pub unpriced_tokens_30d: i64,
    /// Part of `cost_30d` priced from a third-party estimate.
    pub third_party_usd_30d: f64,
    #[ts(type = "number")]
    pub sessions_30d: i64,
    pub last_day: Option<String>,
}

#[derive(Debug, Clone, Serialize, TS, FromRow)]
#[ts(export)]
pub struct UsageReminderRow {
    #[ts(type = "number")]
    pub reminder_id: i64,
    pub title: Option<String>,
    pub cron: Option<String>,
    pub status: Option<String>,
    pub created_by: Option<String>,
    #[sqlx(flatten)]
    pub totals: UsageTotals,
    /// Fire sessions in the selected range.
    #[ts(type = "number")]
    pub sessions: i64,
    pub cost_30d: f64,
    /// Tokens in the last 30 days without a price (not in `cost_30d`).
    #[ts(type = "number")]
    pub unpriced_tokens_30d: i64,
    /// Part of `cost_30d` priced from a third-party estimate.
    pub third_party_usd_30d: f64,
    #[ts(type = "number")]
    pub sessions_30d: i64,
    pub last_day: Option<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct UsageNucleus {
    pub agents: Vec<UsageAgentRow>,
    pub reminders: Vec<UsageReminderRow>,
}

#[derive(Debug, Clone, Serialize, TS, FromRow)]
#[ts(export)]
pub struct UsageLimitEvent {
    pub ts: String,
    pub day: String,
    pub vendor: String,
    pub session_id: String,
    pub project: Option<String>,
    pub agent: Option<String>,
    pub kind: String,
    #[ts(type = "number | null")]
    pub status: Option<i64>,
    pub limit_type: Option<String>,
    pub resets_at: Option<String>,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, TS, FromRow)]
#[ts(export)]
pub struct UsageRatePoint {
    pub day: String,
    pub slot: String,
    pub max_used_percent: f64,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct UsageLimits {
    /// Inclusive local-day bounds of the range, in `timezone`
    /// (`NUCLEUS_TZ`). For all time, `from` is the first event day.
    pub from: String,
    pub to: String,
    pub timezone: String,
    pub events: Vec<UsageLimitEvent>,
    pub codex_daily: Vec<UsageRatePoint>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct UsageSessionRow {
    pub session_id: String,
    pub vendor: String,
    pub project: Option<String>,
    pub agent: Option<String>,
    pub reminder_title: Option<String>,
    pub title: Option<String>,
    pub first_ts: String,
    pub last_ts: String,
    pub totals: UsageTotals,
    #[ts(type = "number")]
    pub subagents: i64,
    pub transcript_path: Option<String>,
    /// Whether the transcript file still exists.
    pub transcript_exists: bool,
}

#[derive(Debug, Clone, Serialize, TS, FromRow)]
#[ts(export)]
pub struct UsagePrice {
    pub model: String,
    pub matched_key: Option<String>,
    /// `list-price`, `third-party-estimate` or `nucleus.toml`.
    pub basis: Option<String>,
    pub source_url: Option<String>,
    /// Date the source was read.
    pub retrieved: Option<String>,
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write_5m: Option<f64>,
    pub cache_write_1h: Option<f64>,
    /// The cache-write rates are derived, not listed by the source.
    pub cache_write_inferred: bool,
    /// The cache-read rate is derived, not listed by the source.
    pub cache_read_inferred: bool,
    /// Whole-request rates above this many input tokens (OpenAI).
    #[ts(type = "number | null")]
    pub long_context_above: Option<i64>,
    pub lc_input: Option<f64>,
    pub lc_output: Option<f64>,
    pub lc_cache_read: Option<f64>,
    pub lc_cache_write: Option<f64>,
}

#[derive(Debug, Clone, Serialize, TS, FromRow)]
#[ts(export)]
pub struct UsageRefreshRun {
    pub started_at: String,
    pub finished_at: Option<String>,
    #[ts(type = "number")]
    pub files_seen: i64,
    #[ts(type = "number")]
    pub files_read: i64,
    /// Files that could not be read in this run.
    #[ts(type = "number")]
    pub files_failed: i64,
    #[ts(type = "number")]
    pub bytes_read: i64,
    #[ts(type = "number")]
    pub rows_written: i64,
    /// Relevant lines that were not valid JSON, in the bytes read.
    #[ts(type = "number")]
    pub malformed_lines: i64,
    /// Lines over the reader's size limit, skipped, in the bytes read.
    #[ts(type = "number")]
    pub oversized_lines: i64,
    /// JSON array of the first warning messages, when any.
    pub warnings: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, TS)]
#[ts(export)]
pub struct UsageStatus {
    pub has_data: bool,
    pub refreshing: bool,
    pub last_refresh: Option<UsageRefreshRun>,
    pub first_day: Option<String>,
    pub last_day: Option<String>,
    pub timezone: String,
    pub prices_as_of: String,
    pub prices: Vec<UsagePrice>,
    #[ts(type = "number")]
    pub sessions: i64,
    /// Sum of Claude Code's own cost-state estimates, each counter counted
    /// once (see `reconcile.rs`).
    pub cost_state_usd: f64,
    /// Cost-state runs whose window repeated responses of an earlier run.
    #[ts(type = "number")]
    pub cost_state_carried_runs: i64,
    /// Over every stored file: relevant lines skipped as malformed or
    /// oversized. Unlike `last_refresh`, these stay until the file is
    /// rewritten, so they report data missing from the store now.
    #[ts(type = "number")]
    pub malformed_lines_total: i64,
    #[ts(type = "number")]
    pub oversized_lines_total: i64,
    /// Sum of `costUSD − table price` over all cost-state runs: how far the
    /// price table is from Claude Code's prices on the covered tokens.
    pub cost_state_adjustment_usd: f64,
    /// Tokens cost-state saw that no transcript line records.
    #[ts(type = "number")]
    pub residual_tokens: i64,
    /// Local day from which Claude data is complete: Claude Code deletes
    /// transcripts after [`CLAUDE_RETENTION_DAYS`], so days before the first
    /// refresh minus that period only hold the sessions whose files survived.
    /// Codex keeps its logs, so Codex history has no such boundary.
    pub claude_complete_since: Option<String>,
}

/// Claude Code's default transcript retention (`cleanupPeriodDays`).
pub const CLAUDE_RETENTION_DAYS: i64 = 30;

/// Tool filter for every view (`?vendor=all|claude|codex`). Applied in SQL, so
/// totals, comparisons and rankings are computed over the selected tool only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize, Serialize, TS)]
#[serde(rename_all = "lowercase")]
#[ts(export)]
pub enum VendorFilter {
    #[default]
    All,
    Claude,
    Codex,
}

impl VendorFilter {
    /// `AND <col> = '<vendor>'`, or nothing for `All`. The value comes from
    /// the enum, never from request text, so it is safe to inline.
    fn and(self, col: &str) -> String {
        match self {
            VendorFilter::All => String::new(),
            VendorFilter::Claude => format!(" AND {col} = 'claude'"),
            VendorFilter::Codex => format!(" AND {col} = 'codex'"),
        }
    }

    fn includes_codex(self) -> bool {
        self != VendorFilter::Claude
    }
}

fn ms_to_iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

fn secs_to_iso(s: Option<i64>) -> Option<String> {
    s.and_then(|s| chrono::DateTime::from_timestamp(s, 0))
        .map(|d| d.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
}

/// Today in the operator's timezone.
pub fn today_local() -> chrono::NaiveDate {
    chrono::Utc::now().with_timezone(&crate::claude_session::nucleus_tz()).date_naive()
}

fn day(d: chrono::NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

async fn totals_between(pool: &SqlitePool, v: VendorFilter, from: &str, to: &str) -> Result<UsageTotals> {
    let sql = format!("SELECT {TOTALS} {FROM} WHERE u.local_day BETWEEN ?1 AND ?2{}", v.and("u.vendor"));
    Ok(sqlx::query_as(&sql).bind(from).bind(to).fetch_one(pool).await?)
}

async fn compare(
    pool: &SqlitePool,
    v: VendorFilter,
    label: &str,
    cur: (chrono::NaiveDate, chrono::NaiveDate),
    prev: (chrono::NaiveDate, chrono::NaiveDate),
) -> Result<UsageCompare> {
    let (cf, ct, pf, pt) = (day(cur.0), day(cur.1), day(prev.0), day(prev.1));
    Ok(UsageCompare {
        label: label.to_string(),
        current: totals_between(pool, v, &cf, &ct).await?,
        previous: totals_between(pool, v, &pf, &pt).await?,
        current_from: cf,
        current_to: ct,
        previous_from: pf,
        previous_to: pt,
    })
}

/// Inclusive local-day bounds shown for a range: `days` days ending today,
/// or for all time (`days == 0`) from the first day with data (`first`,
/// today when there is none). Computed on the server in `NUCLEUS_TZ`, so
/// the page never mixes in the browser's timezone.
pub fn range_bounds(today: chrono::NaiveDate, days: u32, first: Option<&str>) -> (String, String) {
    let to = day(today);
    let from = if days == 0 {
        first.filter(|f| *f <= to.as_str()).map(String::from).unwrap_or_else(|| to.clone())
    } else {
        range_start(today, days)
    };
    (from, to)
}

/// First local day of a `days`-long range ending today; `days == 0` = all.
fn range_start(today: chrono::NaiveDate, days: u32) -> String {
    if days == 0 {
        "0000-01-01".to_string()
    } else {
        day(today - chrono::Duration::days(days as i64 - 1))
    }
}

pub async fn status(pool: Option<&SqlitePool>, workspace_root: &std::path::Path) -> Result<UsageStatus> {
    let refreshing = super::refresh_running(workspace_root);
    let tz = crate::claude_session::nucleus_tz().name().to_string();
    let Some(pool) = pool else {
        return Ok(UsageStatus {
            has_data: false,
            refreshing,
            last_refresh: None,
            first_day: None,
            last_day: None,
            timezone: tz,
            prices_as_of: super::pricing::PRICES_AS_OF.to_string(),
            prices: vec![],
            sessions: 0,
            cost_state_usd: 0.0,
            cost_state_carried_runs: 0,
            malformed_lines_total: 0,
            oversized_lines_total: 0,
            cost_state_adjustment_usd: 0.0,
            residual_tokens: 0,
            claude_complete_since: None,
        });
    };
    let last_refresh: Option<UsageRefreshRun> = sqlx::query_as(
        "SELECT started_at, finished_at, files_seen, files_read, files_failed, bytes_read, rows_written,
                malformed_lines, oversized_lines, warnings, error
           FROM refresh_runs WHERE finished_at IS NOT NULL ORDER BY id DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    let (first_day, last_day): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT MIN(local_day), MAX(local_day) FROM usage_rows").fetch_one(pool).await?;
    let sessions: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sessions").fetch_one(pool).await?;
    let prices: Vec<UsagePrice> = sqlx::query_as(
        "SELECT model, matched_key, basis, source_url, retrieved, input, output, cache_read, cache_write_5m,
                cache_write_1h, cache_write_inferred, cache_read_inferred, long_context_above, lc_input,
                lc_output, lc_cache_read, lc_cache_write
           FROM prices ORDER BY matched_key IS NULL DESC, model",
    )
    .fetch_all(pool)
    .await?;
    let meta_num = |v: Option<String>| v.and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
    let cost_state_usd = meta_num(super::store::meta_get(pool, "cost_state_usd").await?);
    let cost_state_carried_runs = meta_num(super::store::meta_get(pool, "cost_state_carried_runs").await?) as i64;
    let (malformed_lines_total, oversized_lines_total): (i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(malformed_lines),0), COALESCE(SUM(oversized_lines),0) FROM source_files",
    )
    .fetch_one(pool)
    .await?;
    let (adj, residual): (f64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(CASE WHEN kind='adjustment' THEN cost_usd ELSE 0.0 END),0.0),
                COALESCE(SUM(CASE WHEN kind='residual'
                                  THEN input+cache_write_5m+cache_write_1h+cache_read+output ELSE 0 END),0)
           FROM usage_rows WHERE kind IN ('adjustment','residual')",
    )
    .fetch_one(pool)
    .await?;
    let claude_complete_since = super::store::meta_get(pool, "first_refresh")
        .await?
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
        .map(|t| {
            let d = t.with_timezone(&crate::claude_session::nucleus_tz()).date_naive()
                - chrono::Duration::days(CLAUDE_RETENTION_DAYS);
            day(d)
        });
    let prices_as_of = super::store::meta_get(pool, "prices_as_of")
        .await?
        .unwrap_or_else(|| super::pricing::PRICES_AS_OF.to_string());
    Ok(UsageStatus {
        has_data: first_day.is_some(),
        refreshing,
        last_refresh,
        first_day,
        last_day,
        timezone: tz,
        prices_as_of,
        prices,
        sessions,
        cost_state_usd,
        cost_state_carried_runs,
        malformed_lines_total,
        oversized_lines_total,
        cost_state_adjustment_usd: adj,
        residual_tokens: residual,
        claude_complete_since,
    })
}

pub async fn summary(pool: &SqlitePool, days: u32, v: VendorFilter) -> Result<UsageSummary> {
    let today = today_local();
    let d1 = chrono::Duration::days(1);
    let week_start = today - chrono::Duration::days(today.weekday().num_days_from_monday() as i64);
    let n = days.max(1) as i64;
    let range_from = today - chrono::Duration::days(n - 1);

    let day_cmp = compare(pool, v, "today vs yesterday", (today, today), (today - d1, today - d1)).await?;
    let week_cmp = compare(
        pool,
        v,
        "this week vs the same days last week",
        (week_start, today),
        (week_start - chrono::Duration::days(7), today - chrono::Duration::days(7)),
    )
    .await?;
    let range_cmp = if days == 0 {
        let all = totals_between(pool, v, "0000-01-01", &day(today)).await?;
        UsageCompare {
            label: "all time".to_string(),
            current: all,
            previous: UsageTotals::default(),
            current_from: "0000-01-01".to_string(),
            current_to: day(today),
            previous_from: String::new(),
            previous_to: String::new(),
        }
    } else {
        compare(
            pool,
            v,
            &format!("last {n} days vs the {n} days before"),
            (range_from, today),
            (range_from - chrono::Duration::days(n), range_from - d1),
        )
        .await?
    };

    let from = range_start(today, days);
    let to = day(today);
    let daily: Vec<UsageSeriesPoint> = sqlx::query_as(&format!(
        "SELECT local_day AS bucket, vendor,
                COALESCE(SUM(input+cache_write_5m+cache_write_1h+cache_read+output),0) AS tokens,
                COALESCE(SUM(cache_read),0) AS cache_read, COALESCE(SUM(cost_usd),0.0) AS cost_usd
           FROM usage_rows WHERE local_day BETWEEN ?1 AND ?2{}
          GROUP BY local_day, vendor ORDER BY local_day",
        v.and("vendor")
    ))
    .bind(&from)
    .bind(&to)
    .fetch_all(pool)
    .await?;
    // Weekly series spans at least 12 weeks so short ranges still show trend.
    let weekly_from = if days == 0 {
        from.clone()
    } else {
        day((today - chrono::Duration::days((n - 1).max(83))).min(week_start))
    };
    let weekly: Vec<UsageSeriesPoint> = sqlx::query_as(&format!(
        "SELECT date(local_day, '-6 days', 'weekday 1') AS bucket, vendor,
                COALESCE(SUM(input+cache_write_5m+cache_write_1h+cache_read+output),0) AS tokens,
                COALESCE(SUM(cache_read),0) AS cache_read, COALESCE(SUM(cost_usd),0.0) AS cost_usd
           FROM usage_rows WHERE local_day BETWEEN ?1 AND ?2{}
          GROUP BY bucket, vendor ORDER BY bucket",
        v.and("vendor")
    ))
    .bind(&weekly_from)
    .bind(&to)
    .fetch_all(pool)
    .await?;

    let models: Vec<UsageModelRow> = sqlx::query_as(&format!(
        "SELECT u.vendor AS vendor, u.model AS model, {TOTALS}, COUNT(DISTINCT u.session_id) AS sessions
           {FROM} WHERE u.local_day BETWEEN ?1 AND ?2{vf}
          GROUP BY u.vendor, u.model ORDER BY cost_usd DESC, tokens DESC",
        vf = v.and("u.vendor")
    ))
    .bind(&from)
    .bind(&to)
    .fetch_all(pool)
    .await?;
    let by_vendor: Vec<UsageVendorTotals> = sqlx::query_as(&format!(
        "SELECT u.vendor AS vendor, {TOTALS}, COUNT(DISTINCT u.session_id) AS sessions
           {FROM} WHERE u.local_day BETWEEN ?1 AND ?2{vf} GROUP BY u.vendor ORDER BY u.vendor",
        vf = v.and("u.vendor")
    ))
    .bind(&from)
    .bind(&to)
    .fetch_all(pool)
    .await?;
    let heatmap: Vec<UsageHeatCell> = sqlx::query_as(&format!(
        "SELECT local_dow AS dow, local_hour AS hour,
                COALESCE(SUM(input+cache_write_5m+cache_write_1h+cache_read+output),0) AS tokens,
                COALESCE(SUM(cost_usd),0.0) AS cost_usd
           FROM usage_rows WHERE local_day BETWEEN ?1 AND ?2 AND kind = 'response'{}
          GROUP BY local_dow, local_hour",
        v.and("vendor")
    ))
    .bind(&from)
    .bind(&to)
    .fetch_all(pool)
    .await?;

    let first: Option<String> = if days == 0 {
        sqlx::query_scalar(&format!("SELECT MIN(local_day) FROM usage_rows WHERE 1=1{}", v.and("vendor")))
            .fetch_one(pool)
            .await?
    } else {
        None
    };
    let (range_from, _) = range_bounds(today, days, first.as_deref());

    Ok(UsageSummary {
        days,
        range_from,
        today: to,
        by_vendor,
        day: day_cmp,
        week: week_cmp,
        range: range_cmp,
        daily,
        weekly,
        models,
        heatmap,
        codex_quota: if v.includes_codex() { codex_quota(pool).await? } else { vec![] },
    })
}

async fn codex_quota(pool: &SqlitePool) -> Result<Vec<UsageRateReading>> {
    let rows: Vec<(String, f64, Option<i64>, Option<i64>, i64, Option<String>)> = sqlx::query_as(
        "SELECT slot, used_percent, window_minutes, resets_at, ts_ms, plan_type FROM rate_snapshots r
          WHERE vendor = 'codex'
            AND ts_ms = (SELECT MAX(ts_ms) FROM rate_snapshots r2 WHERE r2.vendor = 'codex' AND r2.slot = r.slot)
          GROUP BY slot ORDER BY slot",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(slot, used, win, resets, ts, plan)| UsageRateReading {
            slot,
            used_percent: used,
            window_minutes: win,
            resets_at: secs_to_iso(resets),
            read_at: ms_to_iso(ts),
            plan_type: plan,
        })
        .collect())
}

pub async fn projects(pool: &SqlitePool, days: u32, v: VendorFilter) -> Result<Vec<UsageProjectRow>> {
    let today = today_local();
    let sql = format!(
        "SELECT COALESCE(s.project_name, '(unknown)') AS project, s.project_root AS root, {TOTALS},
                COALESCE(SUM(CASE WHEN u.vendor='claude' THEN u.cost_usd ELSE 0.0 END),0.0) AS claude_cost_usd,
                COALESCE(SUM(CASE WHEN u.vendor='codex' THEN u.cost_usd ELSE 0.0 END),0.0) AS codex_cost_usd,
                COALESCE(SUM(CASE WHEN u.vendor='claude' THEN u.input+u.cache_write_5m+u.cache_write_1h+u.cache_read+u.output ELSE 0 END),0) AS claude_tokens,
                COALESCE(SUM(CASE WHEN u.vendor='codex' THEN u.input+u.cache_write_5m+u.cache_write_1h+u.cache_read+u.output ELSE 0 END),0) AS codex_tokens,
                COUNT(DISTINCT u.session_id) AS sessions, MAX(u.local_day) AS last_day
           {FROM} LEFT JOIN sessions s ON s.session_id = u.session_id
          WHERE u.local_day BETWEEN ?1 AND ?2{vf}
          GROUP BY s.project_root ORDER BY cost_usd DESC, tokens DESC",
        vf = v.and("u.vendor")
    );
    Ok(sqlx::query_as(&sql)
        .bind(range_start(today, days))
        .bind(day(today))
        .fetch_all(pool)
        .await?)
}

/// `workspace_root`: the Nucleus checkout. Its sessions without a label
/// (`unlabeled`) are the operator's interactive sessions plus bot sessions
/// whose label rotated out of every source before a refresh copied it.
pub async fn nucleus(pool: &SqlitePool, days: u32, workspace_root: &str, v: VendorFilter) -> Result<UsageNucleus> {
    let vf = v.and("u.vendor");
    let today = today_local();
    let from = range_start(today, days);
    let to = day(today);
    let from30 = day(today - chrono::Duration::days(29));
    let agents: Vec<UsageAgentRow> = sqlx::query_as(&format!(
        "SELECT COALESCE(s.agent, 'unlabeled') AS agent, {TOTALS},
                COUNT(DISTINCT CASE WHEN u.local_day BETWEEN ?1 AND ?2 THEN u.session_id END) AS sessions,
                0.0 AS cost_30d, 0 AS unpriced_tokens_30d, 0.0 AS third_party_usd_30d, 0 AS sessions_30d,
                MAX(u.local_day) AS last_day
           FROM usage_rows u LEFT JOIN prices p ON p.model = u.model
           JOIN sessions s ON s.session_id = u.session_id
          WHERE (s.agent IS NOT NULL OR s.project_root = ?3) AND u.local_day BETWEEN ?1 AND ?2{vf}
          GROUP BY COALESCE(s.agent, 'unlabeled') ORDER BY cost_usd DESC"
    ))
    .bind(&from)
    .bind(&to)
    .bind(workspace_root)
    .fetch_all(pool)
    .await?;
    let agents_30d: Vec<(String, f64, i64, i64, f64)> = sqlx::query_as(&format!(
        "SELECT COALESCE(s.agent, 'unlabeled'), COALESCE(SUM(u.cost_usd),0.0), COUNT(DISTINCT u.session_id),
                {UNPRICED} AS unpriced, {THIRD_PARTY} AS third_party
           FROM usage_rows u LEFT JOIN prices p ON p.model = u.model
           JOIN sessions s ON s.session_id = u.session_id
          WHERE (s.agent IS NOT NULL OR s.project_root = ?2) AND u.local_day >= ?1{vf}
          GROUP BY COALESCE(s.agent, 'unlabeled')"
    ))
    .bind(&from30)
    .bind(workspace_root)
    .fetch_all(pool)
    .await?;
    let agents = agents
        .into_iter()
        .map(|mut a| {
            if let Some((_, c, n, unpriced, third)) = agents_30d.iter().find(|r| r.0 == a.agent) {
                a.cost_30d = *c;
                a.sessions_30d = *n;
                a.unpriced_tokens_30d = *unpriced;
                a.third_party_usd_30d = *third;
            }
            a
        })
        .collect();

    let reminders: Vec<UsageReminderRow> = sqlx::query_as(&format!(
        "SELECT s.reminder_id AS reminder_id, m.title AS title, m.cron AS cron, m.status AS status,
                m.created_by AS created_by, {TOTALS},
                COUNT(DISTINCT CASE WHEN u.local_day BETWEEN ?1 AND ?2 THEN u.session_id END) AS sessions,
                COALESCE(SUM(CASE WHEN u.local_day >= ?3 THEN u.cost_usd ELSE 0.0 END),0.0) AS cost_30d,
                COALESCE(SUM(CASE WHEN u.local_day >= ?3 AND p.matched_key IS NULL AND u.kind != 'adjustment'
                                  THEN u.input + u.cache_write_5m + u.cache_write_1h + u.cache_read + u.output
                                  ELSE 0 END),0) AS unpriced_tokens_30d,
                COALESCE(SUM(CASE WHEN u.local_day >= ?3 AND p.basis = 'third-party-estimate'
                                  THEN u.cost_usd ELSE 0.0 END),0.0) AS third_party_usd_30d,
                COUNT(DISTINCT CASE WHEN u.local_day >= ?3 THEN u.session_id END) AS sessions_30d,
                MAX(u.local_day) AS last_day
           FROM usage_rows u LEFT JOIN prices p ON p.model = u.model
           JOIN sessions s ON s.session_id = u.session_id
           LEFT JOIN reminders_meta m ON m.reminder_id = s.reminder_id
          WHERE s.reminder_id IS NOT NULL AND (u.local_day BETWEEN ?1 AND ?2 OR u.local_day >= ?3){vf}
          GROUP BY s.reminder_id ORDER BY cost_30d DESC, cost_usd DESC"
    ))
    .bind(&from)
    .bind(&to)
    .bind(&from30)
    .fetch_all(pool)
    .await?;
    // The totals above span range ∪ last 30 days; restate them for the range.
    let mut out = Vec::with_capacity(reminders.len());
    for mut r in reminders {
        let sql = format!(
            "SELECT {TOTALS} {FROM} JOIN sessions s ON s.session_id = u.session_id
              WHERE s.reminder_id = ?1 AND u.local_day BETWEEN ?2 AND ?3{vf}"
        );
        r.totals = sqlx::query_as(&sql).bind(r.reminder_id).bind(&from).bind(&to).fetch_one(pool).await?;
        out.push(r);
    }
    Ok(UsageNucleus { agents, reminders: out })
}

pub async fn limits(pool: &SqlitePool, days: u32, v: VendorFilter) -> Result<UsageLimits> {
    let today = today_local();
    let first: Option<String> = if days == 0 {
        sqlx::query_scalar(&format!("SELECT MIN(e.local_day) FROM limit_events e WHERE 1=1{}", v.and("e.vendor")))
            .fetch_one(pool)
            .await?
    } else {
        None
    };
    let (from, to) = range_bounds(today, days, first.as_deref());
    // A limit event repeated in several files (a fork copies the lines) is
    // shown once: the copy from the first path.
    let rows: Vec<(i64, String, String, String, Option<String>, Option<String>, String, Option<i64>, Option<String>, Option<i64>, Option<String>)> =
        sqlx::query_as(&format!(
            "SELECT e.ts_ms, e.local_day, e.vendor, e.session_id, s.project_name, s.agent, e.kind,
                    e.status, e.limit_type, e.resets_at, e.message
               FROM limit_events e
               JOIN (SELECT key, MIN(path) AS path FROM limit_events GROUP BY key) k
                 ON k.key = e.key AND k.path = e.path
               LEFT JOIN sessions s ON s.session_id = e.session_id
              WHERE e.local_day BETWEEN ?1 AND ?2{} ORDER BY e.ts_ms DESC LIMIT 500",
            v.and("e.vendor")
        ))
        .bind(&from)
        .bind(&to)
        .fetch_all(pool)
        .await?;
    let events = rows
        .into_iter()
        .map(|(ts, day, vendor, session_id, project, agent, kind, status, limit_type, resets, message)| UsageLimitEvent {
            ts: ms_to_iso(ts),
            day,
            vendor,
            session_id,
            project,
            agent,
            kind,
            status,
            limit_type,
            resets_at: secs_to_iso(resets),
            message,
        })
        .collect();

    // Codex readings carry no local day; bucket by UTC-shifted local day in
    // Rust (few thousand rows at most per range).
    let tz = crate::claude_session::nucleus_tz();
    let local = super::store::Local { tz };
    let snaps: Vec<(String, f64, i64)> = if !v.includes_codex() {
        vec![]
    } else {
        sqlx::query_as(
        "SELECT slot, used_percent, ts_ms FROM rate_snapshots WHERE vendor = 'codex' ORDER BY ts_ms",
        )
        .fetch_all(pool)
        .await?
    };
    let mut daily: std::collections::BTreeMap<(String, String), f64> = Default::default();
    for (slot, used, ts) in snaps {
        let d = local.fields(ts).0;
        if d.as_str() < from.as_str() || d.as_str() > to.as_str() {
            continue;
        }
        let e = daily.entry((d, slot)).or_insert(0.0);
        *e = e.max(used);
    }
    let codex_daily = daily
        .into_iter()
        .map(|((day, slot), max_used_percent)| UsageRatePoint { day, slot, max_used_percent })
        .collect();
    Ok(UsageLimits { from, to, timezone: tz.name().to_string(), events, codex_daily })
}

pub async fn sessions(pool: &SqlitePool, days: u32, limit: u32, v: VendorFilter) -> Result<Vec<UsageSessionRow>> {
    let today = today_local();
    let sql = format!(
        "SELECT u.session_id AS session_id, s.vendor AS vendor, s.project_name AS project, s.agent AS agent,
                m.title AS reminder_title, COALESCE(s.custom_title, s.ai_title) AS title,
                MIN(u.ts_ms) AS first_ms, MAX(u.ts_ms) AS last_ms, {TOTALS},
                (SELECT COUNT(*) FROM subagents a WHERE a.session_id = u.session_id) AS subagents,
                s.transcript_path AS transcript_path
           {FROM} JOIN sessions s ON s.session_id = u.session_id
           LEFT JOIN reminders_meta m ON m.reminder_id = s.reminder_id
          WHERE u.local_day BETWEEN ?1 AND ?2{vf}
          GROUP BY u.session_id ORDER BY cost_usd DESC, tokens DESC LIMIT ?3",
        vf = v.and("u.vendor")
    );
    #[derive(FromRow)]
    struct Row {
        session_id: String,
        vendor: String,
        project: Option<String>,
        agent: Option<String>,
        reminder_title: Option<String>,
        title: Option<String>,
        first_ms: i64,
        last_ms: i64,
        #[sqlx(flatten)]
        totals: UsageTotals,
        subagents: i64,
        transcript_path: Option<String>,
    }
    let rows: Vec<Row> = sqlx::query_as(&sql)
        .bind(range_start(today, days))
        .bind(day(today))
        .bind(limit as i64)
        .fetch_all(pool)
        .await?;
    Ok(rows
        .into_iter()
        .map(|r| UsageSessionRow {
            transcript_exists: r.transcript_path.as_deref().is_some_and(|p| std::path::Path::new(p).exists()),
            session_id: r.session_id,
            vendor: r.vendor,
            project: r.project,
            agent: r.agent,
            reminder_title: r.reminder_title,
            title: r.title,
            first_ts: ms_to_iso(r.first_ms),
            last_ts: ms_to_iso(r.last_ms),
            totals: r.totals,
            subagents: r.subagents,
            transcript_path: r.transcript_path,
        })
        .collect())
}

use chrono::Datelike;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_bounds_are_local_days_from_the_server() {
        let today = chrono::NaiveDate::from_ymd_opt(2026, 9, 24).unwrap();
        assert_eq!(range_bounds(today, 7, None), ("2026-09-18".into(), "2026-09-24".into()));
        assert_eq!(range_bounds(today, 1, None), ("2026-09-24".into(), "2026-09-24".into()));
        // All time: from the first day with data, today when there is none,
        // never after today.
        assert_eq!(range_bounds(today, 0, Some("2026-03-02")).0, "2026-03-02");
        assert_eq!(range_bounds(today, 0, None).0, "2026-09-24");
        assert_eq!(range_bounds(today, 0, Some("2026-10-01")).0, "2026-09-24");
    }
}
