//! The transcript sweep and the backfill (ADR-038).
//!
//! Both passes take sessions oldest first, read each from its ledger
//! position to the end of its last completed turn, split what is new into
//! parts, run the pre-filter on every part, and review the parts that pass,
//! one review session at a time, so each review sees the suggestions and
//! candidate workflows the previous one filed. The ledger moves past a part
//! once it is filtered or reviewed.
//!
//! A session in use is read like any other: Claude Code writes a
//! `turn_duration` record when a turn ends, and reading stops after the
//! last one ([`reading_end`]). What follows it — a turn still running, the
//! bots' 04:00 `/compact`, the records Claude Code writes when a session
//! closes — stays for a later run. A session that runs for weeks gets each
//! day's completed turns reviewed the next night.
//!
//! - **Live**: transcripts modified since the live start. At most
//!   `live_max_reviews` reviews per run.
//! - **Backfill**: older transcripts and index-only sessions. At most
//!   `backfill_per_night` reviews per run; the pass stops for the night once
//!   more than `backfill_max_new_per_night` suggestions were filed by the
//!   backfill since local midnight. It ends by itself when no old session
//!   has anything left to read.
//!
//! A review that cannot reach its reviewer (spawn failure, usage limit)
//! stops the pass without moving the ledger. A reviewer that answers twice
//! with unusable JSON counts a failure against the part; after
//! [`MAX_FAILURES`] the part is given up.

use crate::parts::{self, Part};
use crate::prefilter;
use crate::review::{self, Backend, Library, ReviewError, Subject, Task};
use crate::source::{self, Candidate};
use anyhow::Result;
use nucleus_core::chore_state;
use nucleus_core::claude_session::{TranscriptEventKind, TranscriptRead, TurnEnd, TurnRole, transcript_events};
use nucleus_core::config::SkillLearnerConfig;
use nucleus_core::session_index;
use nucleus_core::skill_learner::candidates;
use nucleus_core::skill_learner::ledger::{self, Outcome, RunStart, Source};
use nucleus_core::skill_learner::suggestions::{self, SuggestionOrigin};
use nucleus_core::skills::LibraryRoots;
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};

/// Consecutive unusable answers after which a part is given up.
pub const MAX_FAILURES: i64 = 2;

/// A turn whose session wrote nothing with a timestamp for this long will
/// not complete: the session was closed or killed in the middle of it
/// (Claude Code writes no `turn_duration` then). Its records are read like
/// a completed turn. Far above the longest turn a bot allows (ADR-033's
/// ceiling is hours), so a running turn is never cut.
pub const ABANDONED_AFTER_SECS: i64 = 24 * 60 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pass {
    Live,
    Backfill,
}

impl Pass {
    pub fn as_str(self) -> &'static str {
        match self {
            Pass::Live => "live",
            Pass::Backfill => "backfill",
        }
    }

    fn origin(self) -> SuggestionOrigin {
        match self {
            Pass::Live => SuggestionOrigin::Live,
            Pass::Backfill => SuggestionOrigin::Backfill,
        }
    }
}

/// How one pass runs.
#[derive(Debug, Clone, Copy)]
pub struct PassOptions {
    /// Most review sessions in this pass.
    pub limit: u32,
    /// Measure and print, but review nothing and write nothing.
    pub dry_run: bool,
    /// Backfill only: stop once today's backfill suggestions exceed
    /// `backfill_max_new_per_night`. The nightly run sets it; a backfill the
    /// operator starts from the terminal does not, since he reviews it.
    pub daily_cap: bool,
}

/// What a pass did.
#[derive(Debug, Default)]
pub struct PassReport {
    pub pass: &'static str,
    pub sessions: usize,
    pub skipped_agent: usize,
    pub parts_filtered: usize,
    pub reviewed: usize,
    pub failed: usize,
    pub created: Vec<(i64, String)>,
    /// The new suggestion of each revision.
    pub revised: Vec<i64>,
    pub evidence: usize,
    /// Candidate workflows opened: (id, name).
    pub candidates_opened: Vec<(i64, String)>,
    /// Evidence added to open candidates.
    pub candidate_evidence: usize,
    /// Sessions not reached because the pass stopped.
    pub remaining: usize,
    /// Why the pass stopped before the end of its sessions.
    pub stopped: Option<String>,
    /// A review that could not reach its reviewer.
    pub error: Option<String>,
    /// One line per part (dry run) or per review.
    pub lines: Vec<String>,
}

impl PassReport {
    pub fn summary(&self) -> String {
        let mut s = format!(
            "{}: {} session(s) with new completed turns, {} part(s) filtered, {} reviewed, {} failed, {} suggestion(s) filed{}, {} revised, {} evidence added, {} candidate(s) opened{}, {} candidate evidence added; {} skipped by agent",
            self.pass,
            self.sessions,
            self.parts_filtered,
            self.reviewed,
            self.failed,
            self.created.len(),
            ids_named(&self.created),
            self.revised.len(),
            self.evidence,
            self.candidates_opened.len(),
            ids_named(&self.candidates_opened),
            self.candidate_evidence,
            self.skipped_agent,
        );
        if let Some(why) = &self.stopped {
            s.push_str(&format!("; stopped: {why} ({} session(s) left)", self.remaining));
        }
        if let Some(e) = &self.error {
            s.push_str(&format!("; error: {e}"));
        }
        s
    }
}

/// ` (#1 a, #2 b)`, or nothing for an empty list.
pub fn ids_named(list: &[(i64, String)]) -> String {
    if list.is_empty() {
        return String::new();
    }
    format!(" ({})", list.iter().map(|(id, n)| format!("#{id} {n}")).collect::<Vec<_>>().join(", "))
}

/// Everything a pass needs from its caller.
pub struct Env<'a> {
    pub ws: &'a Path,
    pub cfg: &'a SkillLearnerConfig,
    pub roots: &'a LibraryRoots,
    pub pool: &'a SqlitePool,
    /// The session index, read-only; `None` when there is none.
    pub index: Option<&'a SqlitePool>,
    pub transcripts_dir: PathBuf,
    /// The library text a review prompt shows (recomputed per review).
    pub library_summary: &'a (dyn Fn() -> String + Sync),
    /// Unix seconds.
    pub now: i64,
}

/// The live start (see `source::LIVE_START_KEY`), set on the first run.
/// `persist = false` (a dry run, the backlog check) reads it through a
/// read-only connection and writes nothing.
pub async fn live_start(ws: &Path, now: i64, persist: bool) -> Result<i64> {
    let stored = if persist {
        chore_state::watermark(ws, source::LIVE_START_KEY).await?
    } else {
        chore_state::watermark_read_only(ws, source::LIVE_START_KEY).await?
    };
    if let Some(t) = stored.and_then(|v| v.trim().parse::<i64>().ok()) {
        return Ok(t);
    }
    let t = now - source::FIRST_LIVE_WINDOW_SECS;
    if persist {
        chore_state::set_watermark(ws, source::LIVE_START_KEY, &t.to_string()).await?;
    }
    Ok(t)
}

/// Local midnight of today as a `timestamp` value.
fn local_midnight() -> String {
    use chrono::TimeZone;
    let midnight = chrono::Local::now().date_naive().and_hms_opt(0, 0, 0).expect("valid time");
    let local = chrono::Local
        .from_local_datetime(&midnight)
        .earliest()
        .unwrap_or_else(chrono::Local::now);
    nucleus_core::timestamp::to_sortable(&local.to_rfc3339())
}

/// The local date (`NUCLEUS_TZ`) of unix second `t`, `YYYY-MM-DD`.
pub fn local_date(t: i64) -> String {
    let tz = nucleus_core::claude_session::nucleus_tz();
    chrono::DateTime::from_timestamp(t, 0)
        .map(|d| d.with_timezone(&tz).date_naive().to_string())
        .unwrap_or_else(|| "1970-01-01".into())
}

/// Where reading a transcript stops: the line after its last completed
/// turn (its last `turn_duration` record), or 0 when no turn has
/// completed. When nothing with a timestamp was written for
/// [`ABANDONED_AFTER_SECS`] (`mtime` stands in when no record has one),
/// no turn can still be running and the whole file is read.
pub fn reading_end(read: &TranscriptRead, mtime: i64, now: i64) -> i64 {
    let last_turn = read.turn_ends.last().map(|t| t.line as i64 + 1).unwrap_or(0);
    let activity = read.last_activity.unwrap_or(mtime);
    if now - activity >= ABANDONED_AFTER_SECS { read.complete_lines as i64 } else { last_turn }
}

/// The completed turns that end at or after line `from`: what a run that
/// starts there reviews.
pub fn turns_after(turns: &[TurnEnd], from: i64) -> impl Iterator<Item = &TurnEnd> {
    turns.iter().filter(move |t| t.line as i64 >= from)
}

/// The time a part's work happened: the end of the turn its last line
/// belongs to (the first completed turn ending at or after it), else
/// `fallback`.
fn part_time(turns: &[TurnEnd], part_to: i64, fallback: i64) -> i64 {
    turns.iter().find(|t| t.line as i64 + 1 >= part_to).and_then(|t| t.at).unwrap_or(fallback)
}

/// A session read for one pass.
struct Loaded {
    parts: Vec<Part>,
    opening: Option<String>,
    /// Transcripts: where reading starts and stops, and the file's
    /// complete lines.
    from: i64,
    end: i64,
    complete: i64,
    /// Transcripts: the completed turns in `[from, end)`.
    new_turns: usize,
    /// Local date of each part.
    dates: Vec<String>,
    /// Transcripts: conversation after `end` (a turn still running) that a
    /// later run must read, at the latest when the abandoned-turn rule
    /// applies. While it exists the file's size is not recorded, so the
    /// `unchanged` shortcut never hides it.
    deferred: bool,
}

/// The lines where a new local date starts among the completed turns in
/// `[from, end)`: parts are split there, so a part (and the candidate
/// evidence a review records at it) belongs to one day.
fn day_breaks(turns: &[TurnEnd], from: i64, end: i64) -> Vec<i64> {
    let mut out = Vec::new();
    let mut prev: Option<(i64, String)> = None;
    for t in turns_after(turns, from).filter(|t| (t.line as i64) < end) {
        let Some(at) = t.at else { continue };
        let day = local_date(at);
        if let Some((line, d)) = &prev {
            if *d != day {
                out.push(line + 1);
            }
        }
        prev = Some((t.line as i64, day));
    }
    out
}

/// True when the records at or after `end` hold conversation: operator or
/// assistant text, a tool call or its result. A `/compact`, its summary and
/// the close records are not conversation.
fn deferred_work(read: &TranscriptRead, end: i64) -> bool {
    read.events.iter().any(|e| {
        (e.line as i64) >= end && !matches!(e.kind, TranscriptEventKind::Text { role: TurnRole::User, meta: true, .. })
    })
}

/// The new parts of a candidate, from its ledger position to the end of
/// its last completed turn, with the session's opening request.
async fn load(env: &Env<'_>, c: &Candidate, window: usize) -> Result<Loaded> {
    match c.source {
        Source::Transcript => {
            let path = c.path.clone();
            let read = tokio::task::spawn_blocking(move || transcript_events(&path, 0)).await??;
            let complete = read.complete_lines as i64;
            // Fewer lines than the ledger position: the file was rewritten.
            let from = if c.from > complete { 0 } else { c.from };
            let end = reading_end(&read, c.mtime, env.now).max(from);
            let opening = parts::opening_request(read.events.iter().filter_map(|e| match &e.kind {
                TranscriptEventKind::Text { role: TurnRole::User, text, meta } => Some((text.as_str(), *meta)),
                _ => None,
            }));
            let breaks = day_breaks(&read.turn_ends, from, end);
            let parts = parts::transcript_parts(&read.events, from, end, window, &breaks);
            let fallback = read.last_activity.unwrap_or(c.mtime);
            let dates = parts.iter().map(|p| local_date(part_time(&read.turn_ends, p.to, fallback))).collect();
            let new_turns = turns_after(&read.turn_ends, from).filter(|t| (t.line as i64) < end).count();
            let deferred = deferred_work(&read, end);
            Ok(Loaded { parts, opening, from, end, complete, new_turns, dates, deferred })
        }
        Source::Index => {
            let Some(index) = env.index else {
                return Ok(Loaded {
                    parts: Vec::new(),
                    opening: None,
                    from: c.from,
                    end: c.from,
                    complete: 0,
                    new_turns: 0,
                    dates: Vec::new(),
                    deferred: false,
                });
            };
            let turns = session_index::session_turns(index, &c.session_id, 0).await?;
            let end = c.turn_count.unwrap_or(turns.len() as i64);
            let opening = parts::opening_request(turns.iter().filter(|t| t.role == TurnRole::User).map(|t| (t.text.as_str(), false)));
            let parts = parts::index_parts(&turns, c.from, end, window);
            let dates = vec![local_date(c.mtime); parts.len()];
            Ok(Loaded { parts, opening, from: c.from, end, complete: end, new_turns: 0, dates, deferred: false })
        }
    }
}

/// A pass that stopped on an error, with what it did until then: the
/// suggestions it filed before the error are stored and still need their
/// notice.
#[derive(Debug)]
pub struct PassFailed {
    pub report: Box<PassReport>,
    pub error: anyhow::Error,
}

impl std::fmt::Display for PassFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.error)
    }
}

impl std::error::Error for PassFailed {}

/// Run one pass. Returns its report; a review that could not reach its
/// reviewer is reported in `error`, not as `Err`.
pub async fn run_pass(
    env: &Env<'_>,
    pass: Pass,
    opts: PassOptions,
    backend: &dyn Backend,
) -> std::result::Result<PassReport, PassFailed> {
    let mut report = PassReport { pass: pass.as_str(), ..PassReport::default() };
    match run_pass_into(env, pass, opts, backend, &mut report).await {
        Ok(()) => Ok(report),
        Err(error) => Err(PassFailed { report: Box::new(report), error }),
    }
}

/// The pass, recording into `report` as it goes, so a `?` part-way keeps
/// what was filed before it.
async fn run_pass_into(
    env: &Env<'_>,
    pass: Pass,
    opts: PassOptions,
    backend: &dyn Backend,
    report: &mut PassReport,
) -> Result<()> {
    let ledger_rows = ledger::all(env.pool).await?;
    let labels = source::labels(env.ws).await;
    let index_sessions = match env.index {
        Some(p) => session_index::eligible_sessions(p).await?,
        None => Vec::new(),
    };
    let live_start = live_start(env.ws, env.now, !opts.dry_run).await?;
    let inv = source::inventory(&env.transcripts_dir, env.cfg, &ledger_rows, &labels, &index_sessions, live_start);
    report.skipped_agent = inv.skipped.len();
    if !opts.dry_run {
        for c in &inv.skipped {
            let mark = ledger::Mark { position: 0, file_size: c.size, head_hash: None, text_turns: None };
            ledger::advance(env.pool, &c.session_id, c.source, c.agent.as_deref(), &mark, Outcome::SkippedAgent, None)
                .await?;
        }
    }
    let candidates = match pass {
        Pass::Live => inv.live,
        Pass::Backfill => inv.backfill,
    };
    let window = env.cfg.review_window_bytes.clamp(4 * 1024, review::MAX_WINDOW_BYTES);
    let midnight = local_midnight();

    'sessions: for (i, c) in candidates.iter().enumerate() {
        let loaded = match load(env, c, window).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(session = %c.session_id, "skill sweep: cannot read the session: {e:#}");
                continue;
            }
        };
        if loaded.parts.is_empty() {
            // No completed turn since the position. When the file grew only
            // by records that are not conversation (a `/compact`, close
            // records), note its size so the next pass does not open it
            // again until it grows. A running turn is not cached: the next
            // pass reads it again, and reads it whole once the
            // abandoned-turn rule applies. A rewritten file (read from 0)
            // keeps no size: its ledger position does not belong to it.
            if c.source == Source::Transcript && !opts.dry_run && loaded.from == c.from && !loaded.deferred {
                if let Some(size) = c.size {
                    ledger::record_size(env.pool, &c.session_id, c.from, size).await?;
                }
            }
            continue;
        }
        report.sessions += 1;
        let agent_label = c.agent.as_deref().unwrap_or("terminal");
        if opts.dry_run {
            report.lines.push(match c.source {
                Source::Transcript => format!(
                    "session {} ({agent_label}): lines {}..{} of {}, {} completed turn(s), {} part(s)",
                    c.session_id,
                    loaded.from,
                    loaded.end,
                    loaded.complete,
                    loaded.new_turns,
                    loaded.parts.len()
                ),
                Source::Index => format!(
                    "session {} ({agent_label}, session index): turns {}..{}, {} part(s)",
                    c.session_id,
                    loaded.from,
                    loaded.end,
                    loaded.parts.len()
                ),
            });
        }
        let count = loaded.parts.len();
        for (k, part) in loaded.parts.iter().enumerate() {
            let size = if k + 1 == count && !loaded.deferred { c.size } else { None };
            let signals = serde_json::to_string(&part.signals).ok();
            let mark = ledger::Mark {
                position: part.to,
                file_size: size,
                head_hash: c.head_hash.as_deref(),
                text_turns: Some(part.text_turns),
            };
            let Some(reason) = prefilter::decide(&part.signals, c.source, env.cfg, part.from > 0) else {
                report.parts_filtered += 1;
                if opts.dry_run {
                    report.lines.push(format!("skip   {} [{}..{}) {:?}", c.session_id, part.from, part.to, part.signals));
                } else {
                    ledger::advance(env.pool, &c.session_id, c.source, c.agent.as_deref(), &mark, Outcome::Filtered, signals.as_deref())
                        .await?;
                }
                continue;
            };
            if (report.reviewed + report.failed) as u32 >= opts.limit {
                report.stopped = Some(format!("{} review limit of {} reached", pass.as_str(), opts.limit));
                report.remaining = candidates.len() - i;
                break 'sessions;
            }
            if pass == Pass::Backfill && opts.daily_cap && !opts.dry_run {
                let filed = suggestions::count_created_since(env.pool, SuggestionOrigin::Backfill, &midnight).await?;
                if filed > i64::from(env.cfg.backfill_max_new_per_night) {
                    report.stopped = Some(format!(
                        "{filed} backfill suggestions filed today, above the limit of {}",
                        env.cfg.backfill_max_new_per_night
                    ));
                    report.remaining = candidates.len() - i;
                    break 'sessions;
                }
            }
            if opts.dry_run {
                report.reviewed += 1;
                report.lines.push(format!(
                    "review {} [{}..{}) {agent_label} {} — {reason}",
                    c.session_id, part.from, part.to, loaded.dates[k]
                ));
                continue;
            }

            let open = candidates::open(env.pool).await?;
            let task = Task::Session(Subject {
                session_id: &c.session_id,
                source: c.source,
                agent: c.agent.as_deref(),
                part,
                part_number: k + 1,
                part_count: count,
                opening: loaded.opening.as_deref().filter(|_| part.from > 0),
                reason: &reason,
                local_date: &loaded.dates[k],
                candidates: &open,
                candidate_min_days: env.cfg.candidate_min_days,
            });
            let run_id = ledger::start_run(
                env.pool,
                &RunStart {
                    pass: pass.as_str(),
                    session_id: Some(&c.session_id),
                    source: Some(c.source),
                    from_pos: Some(part.from),
                    to_pos: Some(part.to),
                    candidate_id: None,
                },
            )
            .await?;
            let lib = Library::load(env.pool, env.roots, (env.library_summary)()).await?;
            let result = review::review(backend, env.pool, env.ws, env.roots, &lib, &task, pass.origin()).await;
            match result {
                Ok(o) => {
                    // Recorded first: the suggestions are stored, whatever
                    // the ledger writes below do.
                    report.created.extend(o.created.iter().map(|(id, _, n)| (*id, n.clone())));
                    report.revised.extend(o.revised.iter().map(|(_, new_id)| *new_id));
                    report.candidates_opened.extend(o.candidates_opened.iter().cloned());
                    report.candidate_evidence += o.candidate_evidence.len();
                    let detail = review::describe(&o);
                    ledger::finish_run(
                        env.pool,
                        run_id,
                        "ok",
                        o.review_session_id.as_deref(),
                        o.created.len() as i64,
                        o.evidence_for.len() as i64,
                        Some(&detail),
                    )
                    .await?;
                    ledger::advance(env.pool, &c.session_id, c.source, c.agent.as_deref(), &mark, Outcome::Reviewed, signals.as_deref())
                        .await?;
                    report.reviewed += 1;
                    report.evidence += o.evidence_for.len();
                    report.lines.push(format!("{} [{}..{}): {detail}", c.session_id, part.from, part.to));
                    tracing::info!(session = %c.session_id, from = part.from, to = part.to, "skill review: {detail}");
                }
                Err(ReviewError::Unparsable(e) | ReviewError::TooLarge(e)) => {
                    ledger::finish_run(env.pool, run_id, "unparsable", None, 0, 0, Some(&e)).await?;
                    let failures =
                        ledger::record_failure(env.pool, &c.session_id, c.source, c.agent.as_deref(), part.from).await?;
                    report.failed += 1;
                    report.lines.push(format!("{} [{}..{}): unusable answer ({e})", c.session_id, part.from, part.to));
                    tracing::warn!(session = %c.session_id, failures, "skill review: unusable answer: {e}");
                    if failures >= MAX_FAILURES {
                        ledger::advance(env.pool, &c.session_id, c.source, c.agent.as_deref(), &mark, Outcome::GaveUp, signals.as_deref())
                            .await?;
                    }
                    continue 'sessions;
                }
                Err(e @ ReviewError::Infra(_)) => {
                    let msg = e.to_string();
                    ledger::finish_run(env.pool, run_id, "failed", None, 0, 0, Some(&msg)).await?;
                    report.error = Some(msg);
                    report.stopped = Some("the reviewer could not be reached".into());
                    report.remaining = candidates.len() - i;
                    break 'sessions;
                }
            }
        }
    }
    Ok(())
}

/// A reviewed session whose completed turns wait for review.
#[derive(Debug, Clone, PartialEq)]
pub struct Waiting {
    pub session_id: String,
    pub agent: Option<String>,
    /// Unix seconds: when the oldest waiting turn ended.
    pub oldest: i64,
    /// Completed turns after the ledger position.
    pub turns: usize,
}

/// The live sessions the sweep reviews (not skipped for their agent, not
/// the learner's own) with completed turns after their ledger position,
/// read-only. A turn the abandoned-turn rule completes counts with the
/// time of the newest record. Sessions with nothing waiting are left out.
pub async fn waiting(env: &Env<'_>) -> Result<Vec<Waiting>> {
    let ledger_rows = ledger::all(env.pool).await?;
    let labels = source::labels(env.ws).await;
    let live_start = live_start(env.ws, env.now, false).await?;
    let inv = source::inventory(&env.transcripts_dir, env.cfg, &ledger_rows, &labels, &[], live_start);
    let mut out = Vec::new();
    for c in inv.live.into_iter().filter(|c| c.source == Source::Transcript) {
        let path = c.path.clone();
        let read = match tokio::task::spawn_blocking(move || transcript_events(&path, 0)).await? {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(session = %c.session_id, "backlog: cannot read the transcript: {e}");
                continue;
            }
        };
        let from = if c.from > read.complete_lines as i64 { 0 } else { c.from };
        let end = reading_end(&read, c.mtime, env.now);
        let mut times: Vec<i64> = turns_after(&read.turn_ends, from)
            .filter(|t| (t.line as i64) < end)
            .map(|t| t.at.unwrap_or(c.mtime))
            .collect();
        let tail = read.turn_ends.last().map(|t| t.line as i64 + 1).unwrap_or(0).max(from);
        let operator_text = read.events.iter().any(|e| {
            (e.line as i64) >= tail
                && (e.line as i64) < end
                && matches!(e.kind, TranscriptEventKind::Text { role: TurnRole::User, meta: false, .. })
        });
        if end > tail && operator_text {
            // An abandoned turn, read whole by the next sweep.
            times.push(read.last_activity.unwrap_or(c.mtime));
        }
        if let Some(oldest) = times.iter().copied().min() {
            out.push(Waiting { session_id: c.session_id, agent: c.agent, oldest, turns: times.len() });
        }
    }
    out.sort_by_key(|w| w.oldest);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::ReviewSession;
    use async_trait::async_trait;
    use nucleus_core::skill_learner::suggestions::{SuggestionKind, SuggestionStatus};
    use serde_json::json;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    /// Scripted reviewer: hands out `replies` in order and records prompts.
    #[derive(Default, Clone)]
    struct Fake {
        replies: Arc<Mutex<VecDeque<String>>>,
        prompts: Arc<Mutex<Vec<String>>>,
        opened: Arc<Mutex<usize>>,
        fail_open: bool,
        /// SQL run when a review session closes (before its items are
        /// filed), to make a later step of the pass fail.
        on_close: Option<(SqlitePool, &'static str)>,
    }

    impl Fake {
        fn with(replies: &[&str]) -> Self {
            let f = Fake::default();
            f.replies.lock().unwrap().extend(replies.iter().map(|r| r.to_string()));
            f
        }
        fn prompts(&self) -> Vec<String> {
            self.prompts.lock().unwrap().clone()
        }
    }

    struct FakeSession {
        fake: Fake,
        id: String,
    }

    #[async_trait]
    impl ReviewSession for FakeSession {
        async fn ask(&mut self, prompt: &str) -> Result<String> {
            self.fake.prompts.lock().unwrap().push(prompt.to_string());
            self.fake.replies.lock().unwrap().pop_front().ok_or_else(|| anyhow::anyhow!("no scripted reply"))
        }
        fn session_id(&self) -> Option<String> {
            Some(self.id.clone())
        }
        async fn close(self: Box<Self>) {
            if let Some((pool, sql)) = &self.fake.on_close {
                sqlx::query(sql).execute(pool).await.unwrap();
            }
        }
    }

    #[async_trait]
    impl Backend for Fake {
        async fn open(&self, _window: &str) -> Result<Box<dyn ReviewSession>> {
            if self.fail_open {
                anyhow::bail!("spawn failed");
            }
            let mut n = self.opened.lock().unwrap();
            *n += 1;
            Ok(Box::new(FakeSession { fake: self.clone(), id: format!("rev-{n}") }))
        }
    }

    struct World {
        _tmp: tempfile::TempDir,
        ws: PathBuf,
        dir: PathBuf,
        roots: LibraryRoots,
        pool: SqlitePool,
        cfg: SkillLearnerConfig,
        now: i64,
    }

    async fn world() -> World {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let dir = tmp.path().join("projects");
        std::fs::create_dir_all(ws.join("memory")).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let roots = LibraryRoots::new(&ws, Some(&tmp.path().join("home")));
        let pool = nucleus_core::skill_learner::open(&ws).await.unwrap();
        World { _tmp: tmp, ws, dir, roots, pool, cfg: SkillLearnerConfig::default(), now: chrono::Utc::now().timestamp() }
    }

    fn summary() -> String {
        "(the skill library is currently empty)".into()
    }

    fn env<'a>(w: &'a World, index: Option<&'a SqlitePool>, s: &'a (dyn Fn() -> String + Sync)) -> Env<'a> {
        Env {
            ws: &w.ws,
            cfg: &w.cfg,
            roots: &w.roots,
            pool: &w.pool,
            index,
            transcripts_dir: w.dir.clone(),
            library_summary: s,
            now: w.now,
        }
    }

    async fn pass(w: &World, index: Option<&SqlitePool>, pass: Pass, opts: PassOptions, fake: &Fake) -> PassReport {
        let s = summary;
        run_pass(&env(w, index, &s), pass, opts, fake).await.unwrap()
    }

    fn opts(limit: u32) -> PassOptions {
        PassOptions { limit, dry_run: false, daily_cap: true }
    }

    fn user(t: &str) -> String {
        json!({"type": "user", "message": {"role": "user", "content": t}}).to_string()
    }
    fn asst(t: &str) -> String {
        json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": t}]}}).to_string()
    }
    fn rfc3339(w: &World, secs_ago: i64) -> String {
        chrono::DateTime::from_timestamp(w.now - secs_ago, 0).unwrap().to_rfc3339()
    }
    /// The record Claude Code writes when a turn ends, `secs_ago`.
    fn done(w: &World, secs_ago: i64) -> String {
        json!({"type": "system", "subtype": "turn_duration", "timestamp": rfc3339(w, secs_ago)}).to_string()
    }
    /// An operator prompt with a timestamp, as a live session writes it.
    fn prompt_at(w: &World, t: &str, secs_ago: i64) -> String {
        json!({"type": "user", "origin": {"kind": "human"}, "timestamp": rfc3339(w, secs_ago), "message": {"role": "user", "content": t}}).to_string()
    }
    /// The records of the bots' 04:00 `/compact` outside a turn.
    fn compact(w: &World, secs_ago: i64) -> Vec<String> {
        vec![
            json!({"type": "user", "timestamp": rfc3339(w, secs_ago), "message": {"role": "user", "content": "/compact"}}).to_string(),
            json!({"type": "system", "subtype": "compact_boundary", "timestamp": rfc3339(w, secs_ago)}).to_string(),
            json!({"type": "user", "isCompactSummary": true, "timestamp": rfc3339(w, secs_ago), "message": {"role": "user", "content": "This session is being continued from a previous conversation that ran out of context."}}).to_string(),
            json!({"type": "user", "timestamp": rfc3339(w, secs_ago), "message": {"role": "user", "content": "<command-name>/compact</command-name>"}}).to_string(),
        ]
    }
    /// The records Claude Code writes when a session closes (no timestamp).
    fn close_records() -> Vec<String> {
        vec![json!({"type": "last-prompt", "lastPrompt": "x"}).to_string(), json!({"type": "cost-state", "totalCostUSD": 1.0}).to_string()]
    }

    /// Write (or extend) a transcript and set its mtime `secs_ago`.
    fn transcript(w: &World, sid: &str, lines: &[String], secs_ago: i64) {
        use std::io::Write;
        let p = w.dir.join(format!("{sid}.jsonl"));
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs((w.now - secs_ago) as u64);
        f.set_modified(t).unwrap();
    }

    const DRAFT: &str = "---\nname: x\ndescription: \"Release a build: test first, then push\"\nflavor: learned\ncreated_by: agent\n---\n\n# When to invoke\nWhen releasing.\n\n# Steps\n1. Run the tests.\n2. Push.\n\n# Failure modes\n- Tests fail: stop and report.\n";

    fn create_item(name: &str, draft: &str, quote: &str) -> serde_json::Value {
        json!({"action": "create", "name": name, "rationale": "the operator corrected the release order", "draft": draft, "quotes": [quote, "a quote that is not in the session"]})
    }

    fn reply(items: Vec<serde_json::Value>) -> String {
        format!("```json\n{}\n```", json!({"items": items}))
    }

    #[tokio::test]
    async fn a_pass_that_fails_after_filing_keeps_the_filed_ids() {
        let w = world().await;
        transcript(
            &w,
            "s-old",
            &[user("release the build"), asst("Pushed it."), user("no, you forgot to run the tests first"), asst("Running them now.")],
            10 * 86_400,
        );
        let mut fake = Fake::with(&[&reply(vec![create_item("release-build", DRAFT, "you forgot to run the tests first")])]);
        // The run's ledger row cannot be finished: the pass errors after
        // the review filed its suggestion.
        fake.on_close = Some((w.pool.clone(), "DROP TABLE review_runs"));
        let s = summary;
        let failed = run_pass(&env(&w, None, &s), Pass::Backfill, opts(5), &fake).await.unwrap_err();
        assert_eq!(failed.report.created.len(), 1, "{}", failed.error);
        let id = failed.report.created[0].0;
        assert_eq!(suggestions::get(&w.pool, id).await.unwrap().unwrap().status, SuggestionStatus::Pending);
    }

    #[tokio::test]
    async fn backfill_files_a_suggestion_and_the_live_sweep_reads_only_the_new_part() {
        let w = world().await;
        transcript(
            &w,
            "s-old",
            &[user("release the build"), asst("Pushed it."), user("no, you forgot to run the tests first"), asst("Running them now.")],
            10 * 86_400,
        );
        let fake = Fake::with(&[&reply(vec![create_item("release-build", DRAFT, "you forgot to run the tests first")])]);
        let r = pass(&w, None, Pass::Backfill, opts(5), &fake).await;
        assert_eq!((r.sessions, r.reviewed, r.created.len()), (1, 1, 1), "{}", r.summary());
        let s = suggestions::get(&w.pool, r.created[0].0).await.unwrap().unwrap();
        assert_eq!((s.kind, s.name.as_str(), s.origin), (SuggestionKind::Create, "release-build", SuggestionOrigin::Backfill));
        assert_eq!(s.evidence[0].session_id.as_deref(), Some("s-old"));
        assert_eq!(s.evidence[0].quotes, vec!["you forgot to run the tests first".to_string()], "unverifiable quotes are dropped");
        let prompt = &fake.prompts()[0];
        assert!(prompt.contains("<<<DATA-") && prompt.contains("USER: release the build"));
        assert!(prompt.len() < nucleus_core::claude_session::MAX_TYPED_PROMPT_BYTES);
        assert!(!w.roots.personal.exists(), "suggest mode writes no skill");

        let rows = ledger::all(&w.pool).await.unwrap();
        assert_eq!((rows["s-old"].position, rows["s-old"].outcome.as_str()), (4, "reviewed"));
        assert_eq!(rows["rev-1"].outcome, "own");

        // Nothing new: no review.
        let again = pass(&w, None, Pass::Backfill, opts(5), &fake).await;
        assert_eq!((again.sessions, again.reviewed), (0, 0));

        // The session continues today: the live sweep reads the new lines only.
        transcript(&w, "s-old", &[user("stop pushing before the tests pass"), asst("Understood."), done(&w, 2 * 3600)], 2 * 3600);
        let fake2 = Fake::with(&[&reply(vec![json!({"action": "add_evidence", "suggestion_id": s.id, "note": "same correction again", "quotes": ["stop pushing before the tests pass"]})])]);
        let live = pass(&w, None, Pass::Live, opts(5), &fake2).await;
        assert_eq!((live.reviewed, live.evidence), (1, 1), "{}", live.summary());
        let p = &fake2.prompts()[0];
        assert!(p.contains("stop pushing before the tests pass"));
        assert!(!p.contains("USER: release the build"), "the reviewed part is not sent again");
        assert!(p.contains("opening request of the session (data)>>>\nUSER: release the build") || p.contains("opening request of the session (data)>>>\nrelease the build"), "{p}");
        assert!(p.contains(&format!("#{} create release-build", s.id)), "pending suggestions are shown");
        assert_eq!(suggestions::get(&w.pool, s.id).await.unwrap().unwrap().evidence.len(), 2);
        assert_eq!(ledger::all(&w.pool).await.unwrap()["s-old"].position, 7, "after the turn_duration record");
    }

    #[tokio::test]
    async fn patches_are_edits_and_unseen_quotes_and_bad_drafts_get_one_more_chance() {
        let w = world().await;
        let live_text = DRAFT.replace("name: x", "name: release-build").replace("2. Push.", "2. Push now.");
        std::fs::create_dir_all(w.roots.personal.join("release-build")).unwrap();
        std::fs::write(w.roots.personal.join("release-build/SKILL.md"), &live_text).unwrap();
        transcript(
            &w,
            "s1",
            &[user("ship it"), asst("Shipping."), user("that's not how it works, test before you push"), asst("Ok.")],
            5 * 86_400,
        );
        let broken = DRAFT.replace("# Failure modes\n- Tests fail: stop and report.\n", "");
        let patch = json!({"action": "patch", "name": "release-build", "rationale": "order corrected",
            "edits": [{"old": "2. Push now.\n", "new": "2. Run the tests again.\n3. Push now.\n"}], "quotes": []});
        let fake = Fake::with(&[
            &reply(vec![patch.clone(), create_item("ship-checklist", &broken, "ship it")]),
            &reply(vec![patch, create_item("ship-checklist", DRAFT, "ship it")]),
        ]);
        let r = pass(&w, None, Pass::Backfill, opts(5), &fake).await;
        assert_eq!(r.created.len(), 2, "{}", r.summary());
        let prompts = fake.prompts();
        assert_eq!(prompts.len(), 2);
        assert!(prompts[0].contains("`release-build` —") && prompts[0].contains("# Steps ["), "the outline is shown");
        assert!(!prompts[0].contains("Push now."), "the skill was not loaded in the session");
        assert!(
            prompts[1].contains("section \"# Steps\"") && prompts[1].contains("2. Push now.") && prompts[1].contains("failure modes"),
            "the section of the unseen quote is shown: {}",
            prompts[1]
        );
        let all = suggestions::list(&w.pool, &[SuggestionStatus::Pending]).await.unwrap();
        let p = all.iter().find(|s| s.kind == SuggestionKind::Patch).unwrap();
        let p = suggestions::get(&w.pool, p.id).await.unwrap().unwrap();
        assert_eq!(p.base_hash.as_deref(), Some(nucleus_core::skill_learner::content_hash(live_text.as_bytes()).as_str()));
        assert_eq!(p.draft, live_text.replace("2. Push now.\n", "2. Run the tests again.\n3. Push now.\n"));
        assert_eq!(std::fs::read_to_string(w.roots.personal.join("release-build/SKILL.md")).unwrap(), live_text);
    }

    /// A synthetic skill of about 40 KB: over the create limit, so it could
    /// not be patched as a whole draft.
    fn large_skill() -> String {
        let mut s = String::from(
            "---\nname: big-ops\ndescription: \"Operate the synthetic lab: start, check, stop\"\nflavor: recipe\n---\n\n# When to invoke\nWhen the lab needs work.\n\n# Steps\n1. Start the lab.\n2. Check the lab.\n\n",
        );
        for k in 0..14 {
            s.push_str(&format!("## Service {k}\n"));
            for i in 0..50 {
                s.push_str(&format!("- Service {k} note {i}: a synthetic reference line for sizing.\n"));
            }
            s.push('\n');
        }
        s.push_str("# Failure modes\n- The lab does not start: read its log.\n");
        s
    }

    #[tokio::test]
    async fn a_large_skill_is_patched_by_sections_and_edits_then_revised_and_accepted() {
        let w = world().await;
        let live = large_skill();
        assert!(live.len() > 40 * 1024 && live.len() > suggestions::MAX_DRAFT_BYTES, "{}", live.len());
        std::fs::create_dir_all(w.roots.personal.join("big-ops")).unwrap();
        std::fs::write(w.roots.personal.join("big-ops/SKILL.md"), &live).unwrap();
        transcript(
            &w,
            "s-big",
            &[user("check service 3"), asst("Done."), user("no, you must restart service 3 before the check"), asst("Ok.")],
            5 * 86_400,
        );
        let ask = json!({"items": [], "need_sections": [{"skill": "big-ops", "heading": "## Service 3"}]}).to_string();
        let patch = json!({"items": [{"action": "patch", "name": "big-ops", "rationale": "service 3 needs a restart first",
            "edits": [
                {"old": "- Service 3 note 0: a synthetic reference line for sizing.\n", "new": "- Service 3: restart it before a check.\n- Service 3 note 0: a synthetic reference line for sizing.\n"},
                {"append_to_section": "# Failure modes", "new": "- A check of service 3 fails: restart it first."}
            ],
            "quotes": ["you must restart service 3 before the check"]}]})
        .to_string();
        let fake = Fake::with(&[&ask, &patch]);
        let r = pass(&w, None, Pass::Backfill, opts(5), &fake).await;
        assert_eq!(r.created.len(), 1, "{} {:?}", r.summary(), r.lines);
        let prompts = fake.prompts();
        let limit = nucleus_core::claude_session::MAX_TYPED_PROMPT_BYTES;
        assert!(prompts.iter().all(|p| p.len() < limit));
        assert!(prompts[0].contains("## Service 11 [") && !prompts[0].contains("note 7:"), "outline only");
        assert!(prompts[1].contains("section ## Service 3") && prompts[1].contains("- Service 3 note 49:"));
        assert!(!prompts[1].contains("Service 4 note"), "only the requested section");
        let id = r.created[0].0;
        let expected = live
            .replace(
                "- Service 3 note 0:",
                "- Service 3: restart it before a check.\n- Service 3 note 0:",
            )
            .replace("read its log.\n", "read its log.\n- A check of service 3 fails: restart it first.\n");
        assert_eq!(suggestions::get(&w.pool, id).await.unwrap().unwrap().draft, expected);

        // A later session revises the pending patch: its edits apply to the
        // pending draft, which the outline and the sections now show.
        transcript(&w, "s-big2", &[user("restart service 3"), user("no, wrong: restart it twice"), asst("Ok.")], 4 * 86_400);
        let ask = json!({"items": [], "need_sections": [{"skill": "big-ops", "heading": "# Failure modes"}]}).to_string();
        let revise = json!({"items": [{"action": "revise", "suggestion_id": id, "rationale": "twice",
            "edits": [{"old": "- A check of service 3 fails: restart it first.", "new": "- A check of service 3 fails: restart it twice."}],
            "quotes": ["restart it twice"]}]})
        .to_string();
        let fake = Fake::with(&[&ask, &revise]);
        let r = pass(&w, None, Pass::Backfill, opts(5), &fake).await;
        assert_eq!(r.revised.len(), 1, "{} {:?}", r.summary(), r.lines);
        assert!(fake.prompts()[0].contains(&format!("the text of pending suggestion #{id}")));
        let new_id = r.revised[0];
        let shown = nucleus_core::skill_learner::content_hash(
            suggestions::get(&w.pool, new_id).await.unwrap().unwrap().draft.as_bytes(),
        );
        suggestions::accept(&w.pool, &w.ws, &w.roots, new_id, &shown).await.unwrap();
        let written = std::fs::read_to_string(w.roots.personal.join("big-ops/SKILL.md")).unwrap();
        assert_eq!(written, expected.replace("restart it first.", "restart it twice."));
        assert!(written.contains("flavor: recipe") && !written.contains("created_by"), "ownership kept");
    }

    #[tokio::test]
    async fn an_edit_that_does_not_apply_drops_the_whole_item_with_its_reason() {
        let w = world().await;
        let live_text = DRAFT.replace("name: x", "name: release-build");
        std::fs::create_dir_all(w.roots.personal.join("release-build")).unwrap();
        std::fs::write(w.roots.personal.join("release-build/SKILL.md"), &live_text).unwrap();
        transcript(&w, "s1", &[user("ship it"), user("no, wrong order, test first"), asst("Ok.")], 5 * 86_400);
        let bad = json!({"action": "patch", "name": "release-build", "rationale": "order",
            "edits": [{"old": "1. Run the tests.\n", "new": "1. Run all tests.\n"}, {"old": "3. Tag the release.", "new": "x"}]});
        let fake = Fake::with(&[&reply(vec![bad.clone()]), &reply(vec![bad])]);
        let r = pass(&w, None, Pass::Backfill, opts(5), &fake).await;
        assert_eq!((r.reviewed, r.created.len()), (1, 0), "{}", r.summary());
        assert!(r.lines[0].contains("dropped 1:") && r.lines[0].contains("edit 2: `old` does not occur"), "{:?}", r.lines);
        assert!(suggestions::list(&w.pool, &[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn unusable_answers_fail_the_part_then_give_it_up() {
        let w = world().await;
        transcript(&w, "s1", &[user("deploy"), user("no, wrong host"), asst("ok")], 5 * 86_400);
        for round in 1..=2 {
            let fake = Fake::with(&["I think nothing", "still not json"]);
            let r = pass(&w, None, Pass::Backfill, opts(5), &fake).await;
            assert_eq!((r.reviewed, r.failed), (0, 1), "round {round}");
        }
        let row = &ledger::all(&w.pool).await.unwrap()["s1"];
        assert_eq!((row.outcome.as_str(), row.position), ("gave-up", 3));
        let r = pass(&w, None, Pass::Backfill, opts(5), &Fake::default()).await;
        assert_eq!(r.sessions, 0);
    }

    #[tokio::test]
    async fn an_unreachable_reviewer_stops_the_pass_without_moving_the_ledger() {
        let w = world().await;
        transcript(&w, "s1", &[user("deploy"), user("no, wrong host"), asst("ok")], 5 * 86_400);
        let fake = Fake { fail_open: true, ..Fake::default() };
        let r = pass(&w, None, Pass::Backfill, opts(5), &fake).await;
        assert!(r.error.as_deref().unwrap().contains("spawn failed"));
        assert_eq!(r.remaining, 1);
        assert!(!ledger::all(&w.pool).await.unwrap().contains_key("s1"));
        // A usage-limit banner as the answer is the same failure.
        let fake = Fake::with(&["You've hit your usage limit · resets 5pm"]);
        let r = pass(&w, None, Pass::Backfill, opts(5), &fake).await;
        assert!(r.error.is_some(), "{}", r.summary());
        assert!(!ledger::all(&w.pool).await.unwrap().contains_key("s1"));
    }

    #[tokio::test]
    async fn backfill_limits_reviews_and_new_suggestions_per_night() {
        let mut w = world().await;
        for (i, sid) in ["a", "b", "c"].iter().enumerate() {
            transcript(&w, sid, &[user("build it"), user("no, wrong target"), asst("ok")], (10 + i as i64) * 86_400);
        }
        // Review limit.
        let fake = Fake::with(&[r#"{"items": []}"#]);
        let r = pass(&w, None, Pass::Backfill, opts(1), &fake).await;
        assert_eq!(r.reviewed, 1);
        assert_eq!(r.remaining, 2);
        assert!(r.stopped.unwrap().contains("review limit"));
        // Suggestion limit: above 0 new suggestions today, the backfill stops.
        w.cfg.backfill_max_new_per_night = 0;
        let fake = Fake::with(&[&reply(vec![create_item("build-targets", DRAFT, "wrong target")]), r#"{"items": []}"#]);
        let r = pass(&w, None, Pass::Backfill, opts(5), &fake).await;
        assert_eq!((r.reviewed, r.created.len()), (1, 1));
        assert!(r.stopped.as_deref().unwrap().contains("above the limit"), "{}", r.summary());
        assert_eq!(fake.prompts().len(), 1);
        // A manual backfill is not held to the daily limit.
        let fake = Fake::with(&[r#"{"items": []}"#]);
        let r = pass(&w, None, Pass::Backfill, PassOptions { daily_cap: false, ..opts(5) }, &fake).await;
        assert!(r.reviewed >= 1, "{}", r.summary());
        assert!(!r.stopped.as_deref().unwrap_or("").contains("above the limit"), "{}", r.summary());
    }

    #[tokio::test]
    async fn skipped_filtered_running_and_dry_runs() {
        let w = world().await;
        std::fs::create_dir_all(w.ws.join("memory/logs/reminders-fire")).unwrap();
        std::fs::write(
            w.ws.join("memory/logs/reminders-fire/runs.jsonl"),
            format!("{}\n", json!({"session_id": "fire", "agent": "reminders-fire"})),
        )
        .unwrap();
        transcript(&w, "fire", &[user("run the report"), user("no, wrong"), asst("ok"), done(&w, 3600)], 3600);
        // A turn still running, written a minute ago: nothing completed yet.
        transcript(&w, "running", &[prompt_at(&w, "hi", 60), prompt_at(&w, "no, wrong", 60)], 60);
        transcript(&w, "calm", &[user("what time is it"), asst("noon"), done(&w, 3600)], 3600);
        transcript(&w, "fix", &[user("rename it"), user("wrong file, the other one"), asst("ok"), done(&w, 3600)], 3600);

        let dry = pass(&w, None, Pass::Live, PassOptions { limit: 5, dry_run: true, daily_cap: false }, &Fake::default()).await;
        assert_eq!((dry.skipped_agent, dry.parts_filtered, dry.reviewed), (1, 1, 1), "{}", dry.summary());
        assert!(dry.lines.iter().any(|l| l.starts_with("review fix")));
        assert!(dry.lines.iter().any(|l| l.starts_with("session fix (terminal): lines 0..4 of 4, 1 completed turn(s), 1 part(s)")), "{:?}", dry.lines);
        assert!(!dry.lines.iter().any(|l| l.contains("running")), "{:?}", dry.lines);
        assert!(ledger::all(&w.pool).await.unwrap().is_empty(), "a dry run records nothing");

        let fake = Fake::with(&[r#"{"items": []}"#]);
        let r = pass(&w, None, Pass::Live, opts(5), &fake).await;
        assert_eq!((r.reviewed, r.parts_filtered), (1, 1));
        let rows = ledger::all(&w.pool).await.unwrap();
        assert_eq!(rows["fire"].outcome, "skipped-agent");
        assert_eq!(rows["calm"].outcome, "filtered");
        assert_eq!(rows["fix"].outcome, "reviewed");
        assert!(!rows.contains_key("running"), "a session without a completed turn gets no row");
    }

    #[tokio::test]
    async fn a_session_in_use_is_reviewed_up_to_its_last_completed_turn() {
        let w = world().await;
        // One completed turn with a correction, then a turn still running.
        let turn1 = [prompt_at(&w, "deploy the service", 7200), prompt_at(&w, "no, wrong host, use the staging one", 7100), asst("Deployed to staging."), done(&w, 7000)];
        transcript(&w, "dm", &turn1, 60);
        transcript(&w, "dm", &[prompt_at(&w, "now RUNNING-TURN-TEXT and stop doing that", 60)], 60);
        let fake = Fake::with(&[r#"{"items": []}"#]);
        let r = pass(&w, None, Pass::Live, opts(5), &fake).await;
        assert_eq!((r.sessions, r.reviewed), (1, 1), "{}", r.summary());
        assert!(!fake.prompts()[0].contains("RUNNING-TURN-TEXT"), "a running turn waits");
        let row = ledger::all(&w.pool).await.unwrap()["dm"].clone();
        assert_eq!((row.position, row.outcome.as_str()), (4, "reviewed"));

        // The night's /compact and the close records: no completed turn,
        // no review. The running turn is still there, so no size is noted:
        // the file is read again next time.
        transcript(&w, "dm", &compact(&w, 30), 30);
        transcript(&w, "dm", &close_records(), 30);
        let r = pass(&w, None, Pass::Live, opts(5), &Fake::default()).await;
        assert_eq!((r.sessions, r.reviewed, r.parts_filtered), (0, 0, 0), "{}", r.summary());
        let row = ledger::all(&w.pool).await.unwrap()["dm"].clone();
        assert_eq!((row.position, row.file_size), (4, None));

        // The running turn completes: the next pass reviews everything after
        // the first turn; the /compact line is not an operator turn.
        transcript(&w, "dm", &[asst("Stopped."), done(&w, 10)], 10);
        let fake = Fake::with(&[r#"{"items": []}"#]);
        let r = pass(&w, None, Pass::Live, opts(5), &fake).await;
        assert_eq!(r.reviewed, 1, "{}", r.summary());
        let p = &fake.prompts()[0];
        assert!(p.contains("RUNNING-TURN-TEXT") && !p.contains("USER: /compact"), "{p}");
        let total = std::fs::read_to_string(w.dir.join("dm.jsonl")).unwrap().lines().count() as i64;
        assert_eq!(ledger::all(&w.pool).await.unwrap()["dm"].position, total);

        // The next night's /compact and close records alone: the size is
        // noted, and the pass after that does not open the file.
        transcript(&w, "dm", &compact(&w, 5), 5);
        transcript(&w, "dm", &close_records(), 5);
        let r = pass(&w, None, Pass::Live, opts(5), &Fake::default()).await;
        assert_eq!((r.sessions, r.reviewed), (0, 0), "{}", r.summary());
        let size = std::fs::metadata(w.dir.join("dm.jsonl")).unwrap().len() as i64;
        let row = ledger::all(&w.pool).await.unwrap()["dm"].clone();
        assert_eq!((row.position, row.file_size), (total, Some(size)));
        let s = summary;
        let inv_ledger = ledger::all(&w.pool).await.unwrap();
        let inv = source::inventory(&w.dir, &w.cfg, &inv_ledger, &Default::default(), &[], 0);
        assert!(inv.live.iter().all(|c| c.session_id != "dm"), "unchanged: not opened");
        assert!(waiting(&env(&w, None, &s)).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_running_turn_killed_after_a_sweep_is_read_once_it_is_abandoned() {
        let mut w = world().await;
        let turn1 = [prompt_at(&w, "deploy the service", 7200), prompt_at(&w, "no, wrong host", 7100), asst("ok"), done(&w, 7000)];
        transcript(&w, "killed", &turn1, 60);
        transcript(&w, "killed", &[prompt_at(&w, "rename the files", 60), prompt_at(&w, "no, wrong folder", 60)], 60);
        let r = pass(&w, None, Pass::Live, opts(5), &Fake::with(&[r#"{"items": []}"#])).await;
        assert_eq!(r.reviewed, 1, "{}", r.summary());
        assert_eq!(ledger::all(&w.pool).await.unwrap()["killed"].position, 4);
        // The session is killed; two days later nothing was written since.
        w.now += 2 * 86_400;
        let s = summary;
        assert_eq!(waiting(&env(&w, None, &s)).await.unwrap().len(), 1, "the abandoned turn waits");
        let fake = Fake::with(&[r#"{"items": []}"#]);
        let r = pass(&w, None, Pass::Live, opts(5), &fake).await;
        assert_eq!(r.reviewed, 1, "{}", r.summary());
        assert!(fake.prompts()[0].contains("wrong folder"));
        assert_eq!(ledger::all(&w.pool).await.unwrap()["killed"].position, 6);
    }

    #[tokio::test]
    async fn parts_end_at_a_new_local_day() {
        let w = world().await;
        let lines = [
            prompt_at(&w, "export the numbers", 3 * 86_400),
            prompt_at(&w, "no, wrong format", 3 * 86_400),
            asst("done"),
            done(&w, 3 * 86_400),
            prompt_at(&w, "export them again", 86_400),
            prompt_at(&w, "no, wrong sheet", 86_400),
            asst("done"),
            done(&w, 86_400),
        ];
        transcript(&w, "days", &lines, 3600);
        let fake = Fake::with(&[r#"{"items": []}"#, r#"{"items": []}"#]);
        let r = pass(&w, None, Pass::Live, opts(5), &fake).await;
        assert_eq!(r.reviewed, 2, "{} {:?}", r.summary(), r.lines);
        let p = fake.prompts();
        assert!(p[0].contains("wrong format") && !p[0].contains("wrong sheet"));
        assert!(p[0].contains(&local_date(w.now - 3 * 86_400)) && p[1].contains(&local_date(w.now - 86_400)));
    }

    #[tokio::test]
    async fn a_compact_alone_is_never_reviewed_and_an_abandoned_turn_is_read() {
        let w = world().await;
        // The files were touched an hour ago (close records carry no time);
        // the records themselves are days old.
        transcript(&w, "bot", &[prompt_at(&w, "check the queue", 3 * 86_400), asst("Empty."), done(&w, 3 * 86_400)], 3600);
        let r = pass(&w, None, Pass::Live, opts(5), &Fake::default()).await;
        assert_eq!(r.parts_filtered, 1, "{}", r.summary());
        // A compaction two days ago and nothing since: no turn can still be
        // running, so the records are read, and they are not operator work.
        transcript(&w, "bot", &compact(&w, 2 * 86_400), 3600);
        let r = pass(&w, None, Pass::Live, opts(5), &Fake::default()).await;
        assert_eq!((r.reviewed, r.parts_filtered), (0, 1), "{}", r.summary());
        // A session killed in the middle of a turn two days ago: its turn
        // never completes, and it is read whole.
        transcript(&w, "killed", &[prompt_at(&w, "rename the files", 2 * 86_400), prompt_at(&w, "no, wrong folder", 2 * 86_400)], 3600);
        let fake = Fake::with(&[r#"{"items": []}"#]);
        let r = pass(&w, None, Pass::Live, opts(5), &fake).await;
        assert_eq!(r.reviewed, 1, "{}", r.summary());
        assert_eq!(ledger::all(&w.pool).await.unwrap()["killed"].position, 2);
    }

    #[tokio::test]
    async fn backlog_lists_completed_turns_waiting_for_review() {
        let w = world().await;
        let s = summary;
        transcript(&w, "waits", &[prompt_at(&w, "a", 50 * 3600), asst("b"), done(&w, 50 * 3600), prompt_at(&w, "c", 3600), asst("d"), done(&w, 3600)], 3600);
        transcript(&w, "fresh", &[prompt_at(&w, "a", 3600), asst("b"), done(&w, 3600)], 3600);
        transcript(&w, "running", &[prompt_at(&w, "a", 60)], 60);
        let list = waiting(&env(&w, None, &s)).await.unwrap();
        let ids: Vec<(&str, usize)> = list.iter().map(|x| (x.session_id.as_str(), x.turns)).collect();
        assert_eq!(ids, vec![("waits", 2), ("fresh", 1)], "oldest first; a running turn is not waiting");
        assert_eq!(list[0].oldest, w.now - 50 * 3600);
        assert!(ledger::all(&w.pool).await.unwrap().is_empty(), "the check writes nothing");
        // After the sweep (the parts are filtered) nothing waits.
        pass(&w, None, Pass::Live, opts(5), &Fake::default()).await;
        assert!(waiting(&env(&w, None, &s)).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn repeated_work_becomes_a_candidate_and_code_promotes_it_on_the_second_day() {
        let w = world().await;
        let day1 = [prompt_at(&w, "export the weekly numbers to the shared sheet", 3 * 86_400), prompt_at(&w, "no, wrong format, use the csv one", 3 * 86_400), asst("Exported as csv."), done(&w, 3 * 86_400)];
        transcript(&w, "s-day1", &day1, 3600);
        let first = json!({"items": [], "candidates": [{"name": "weekly-export", "description": "Export the weekly numbers as csv", "note": "asked for the export"}]}).to_string();
        let r = pass(&w, None, Pass::Live, opts(5), &Fake::with(&[&first])).await;
        assert_eq!(r.candidates_opened.len(), 1, "{} {:?}", r.summary(), r.lines);
        assert!(r.summary().contains("1 candidate(s) opened (#1 weekly-export)"), "{}", r.summary());
        let s = summary;
        let e = env(&w, None, &s);
        // One day only: nothing to promote.
        let p = crate::promote::run(&e, 5, false, &Fake::default()).await.unwrap();
        assert!(p.promoted.is_empty() && p.lines.is_empty(), "{:?}", p.lines);

        // Another day, another session: the review sees the open candidate
        // and adds evidence to it.
        let day2 = [prompt_at(&w, "do the weekly export again", 86_400), prompt_at(&w, "no, wrong sheet", 86_400), asst("Done."), done(&w, 86_400)];
        transcript(&w, "s-day2", &day2, 3600);
        let second = json!({"items": [], "candidates": [{"candidate_id": 1, "note": "the export again"}]}).to_string();
        let fake = Fake::with(&[&second]);
        let r = pass(&w, None, Pass::Live, opts(5), &fake).await;
        assert_eq!(r.candidate_evidence, 1, "{}", r.summary());
        assert!(fake.prompts()[0].contains("#1 weekly-export — Export the weekly numbers as csv (seen on"), "{}", fake.prompts()[0]);

        // Promotion: one review with the excerpts of both days.
        let draft = DRAFT.replace("name: x", "name: weekly-export");
        let promote = reply(vec![create_item("weekly-export", &draft, "use the csv one")]);
        let fake = Fake::with(&[&promote]);
        let p = crate::promote::run(&e, 5, false, &fake).await.unwrap();
        assert_eq!(p.promoted.len(), 1, "{:?}", p.lines);
        let prompt = &fake.prompts()[0];
        assert!(prompt.contains("export the weekly numbers to the shared sheet") && prompt.contains("do the weekly export again"), "{prompt}");
        let sid = p.promoted[0].1;
        assert_eq!(p.filed, vec![sid]);
        let s = suggestions::get(&w.pool, sid).await.unwrap().unwrap();
        assert_eq!(s.origin, SuggestionOrigin::Candidate);
        let sessions: Vec<&str> = s.evidence.iter().filter_map(|e| e.session_id.as_deref()).collect();
        assert_eq!(sessions, vec!["s-day1", "s-day2"]);
        assert_eq!(s.evidence[0].quotes, vec!["use the csv one".to_string()]);
        let c = candidates::get(&w.pool, 1).await.unwrap().unwrap();
        assert_eq!((c.status.as_str(), c.suggestion_id), ("promoted", Some(sid)));
        assert!(ledger::all(&w.pool).await.unwrap()["rev-1"].outcome == "own", "promotion sessions are the learner's own");
    }

    #[tokio::test]
    async fn a_transcript_deleted_part_way_continues_from_the_index() {
        let mut w = world().await;
        w.cfg.review_window_bytes = 4 * 1024;
        let long = "x".repeat(3000);
        let turns: Vec<(&str, String)> = vec![
            ("user", format!("first task {long}")),
            ("user", "no, wrong target".into()),
            ("assistant", "ok".into()),
            ("user", format!("second task {long}")),
            ("user", "no, wrong order".into()),
            ("assistant", "done".into()),
        ];
        let lines: Vec<String> = turns.iter().map(|(r, t)| if *r == "user" { user(t) } else { asst(t) }).collect();
        transcript(&w, "half", &lines, 5 * 86_400);
        // The review limit stops after the first part.
        let r = pass(&w, None, Pass::Backfill, opts(1), &Fake::with(&[r#"{"items": []}"#])).await;
        assert_eq!(r.reviewed, 1, "{}", r.summary());
        let row = ledger::all(&w.pool).await.unwrap()["half"].clone();
        assert!(row.position < 6 && row.text_turns.is_some(), "{row:?}");
        // Claude Code deletes the transcript; the index still has its turns.
        std::fs::remove_file(w.dir.join("half.jsonl")).unwrap();
        let index = nucleus_core::session_index::open(&w.ws).await.unwrap();
        sqlx::query(
            "INSERT INTO indexed_sessions (session_id, path, agent, mtime, size, eligible, turn_count, indexed_at)
             VALUES ('half', ?1, NULL, 100, 10, 1, 6, 'x')",
        )
        .bind(w.dir.join("half.jsonl").to_string_lossy().as_ref())
        .execute(&index)
        .await
        .unwrap();
        for (seq, (role, text)) in turns.iter().enumerate() {
            sqlx::query("INSERT INTO turns_fts (text, session_id, agent, role, seq, session_ts) VALUES (?1, 'half', '', ?2, ?3, 'x')")
                .bind(text)
                .bind(role)
                .bind(seq as i64)
                .execute(&index)
                .await
                .unwrap();
        }
        let fake = Fake::with(&[r#"{"items": []}"#]);
        let r = pass(&w, Some(&index), Pass::Backfill, opts(5), &fake).await;
        assert_eq!(r.reviewed, 1, "{}", r.summary());
        let p = &fake.prompts()[0];
        assert!(p.contains("wrong order") && !p.contains("wrong target"), "only the unread turns");
        let row = &ledger::all(&w.pool).await.unwrap()["half"];
        assert_eq!((row.source, row.position), (Source::Index, 6));
    }

    #[tokio::test]
    async fn index_only_sessions_are_reviewed_from_the_session_index() {
        let w = world().await;
        let index = nucleus_core::session_index::open(&w.ws).await.unwrap();
        let gone = w.dir.join("gone.jsonl").to_string_lossy().into_owned();
        sqlx::query(
            "INSERT INTO indexed_sessions (session_id, path, agent, mtime, size, eligible, turn_count, indexed_at)
             VALUES ('gone', ?1, NULL, 100, 10, 1, 3, 'x')",
        )
        .bind(&gone)
        .execute(&index)
        .await
        .unwrap();
        for (seq, role, text) in [(0, "user", "back up the photos"), (1, "assistant", "done"), (2, "user", "não é isso, usa o disco externo")] {
            sqlx::query("INSERT INTO turns_fts (text, session_id, agent, role, seq, session_ts) VALUES (?1, 'gone', '', ?2, ?3, 'x')")
                .bind(text)
                .bind(role)
                .bind(seq)
                .execute(&index)
                .await
                .unwrap();
        }
        let fake = Fake::with(&[r#"{"items": []}"#]);
        let r = pass(&w, Some(&index), Pass::Backfill, opts(5), &fake).await;
        assert_eq!(r.reviewed, 1, "{}", r.summary());
        assert!(fake.prompts()[0].contains("session index: text only"));
        let row = &ledger::all(&w.pool).await.unwrap()["gone"];
        assert_eq!((row.source, row.position), (Source::Index, 3));
        let r = pass(&w, Some(&index), Pass::Backfill, opts(5), &fake).await;
        assert_eq!(r.sessions, 0);
    }
}
