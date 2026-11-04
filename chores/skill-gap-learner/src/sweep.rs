//! The transcript sweep and the backfill (ADR-038).
//!
//! Both passes take sessions oldest first, read each from its ledger
//! position, split what is new into parts, run the pre-filter on every part,
//! and review the parts that pass, one review session at a time, so each
//! review sees the suggestions the previous one filed. The ledger moves past
//! a part once it is filtered or reviewed.
//!
//! - **Live**: transcripts modified since the live start, skipping those
//!   still in use (modified in the last `quiet_minutes`). At most
//!   `live_max_reviews` reviews per run. `live_mode` decides whether a
//!   review files suggestions or writes skills.
//! - **Backfill**: older transcripts and index-only sessions. At most
//!   `backfill_per_night` reviews per run; the pass stops for the night once
//!   more than `backfill_max_new_per_night` suggestions were filed by the
//!   backfill since local midnight. Always suggest mode. It ends by itself
//!   when no old session has anything left to read.
//!
//! A review that cannot reach its reviewer (spawn failure, usage limit)
//! stops the pass without moving the ledger. A reviewer that answers twice
//! with unusable JSON counts a failure against the part; after
//! [`MAX_FAILURES`] the part is given up.

use crate::parts::{self, Part};
use crate::prefilter;
use crate::review::{self, Backend, Library, ReviewError, Subject};
use crate::source::{self, Candidate};
use anyhow::Result;
use nucleus_core::chore_state;
use nucleus_core::claude_session::{TranscriptEventKind, TurnRole, transcript_events};
use nucleus_core::config::SkillLearnerConfig;
use nucleus_core::session_index;
use nucleus_core::skill_learner::ledger::{self, Outcome, RunStart, Source};
use nucleus_core::skill_learner::suggestions::{self, SuggestionOrigin};
use nucleus_core::skills::LibraryRoots;
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};

/// Consecutive unusable answers after which a part is given up.
pub const MAX_FAILURES: i64 = 2;

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
    /// Live pass only: write skills instead of filing suggestions.
    pub write_mode: bool,
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
    pub busy: usize,
    pub skipped_agent: usize,
    pub parts_filtered: usize,
    pub reviewed: usize,
    pub failed: usize,
    pub created: Vec<(i64, String)>,
    /// The new suggestion of each revision.
    pub revised: Vec<i64>,
    pub evidence: usize,
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
            "{}: {} session(s) with new text, {} part(s) filtered, {} reviewed, {} failed, {} suggestion(s) filed{}, {} revised, {} evidence added; {} in use, {} skipped by agent",
            self.pass,
            self.sessions,
            self.parts_filtered,
            self.reviewed,
            self.failed,
            self.created.len(),
            if self.created.is_empty() {
                String::new()
            } else {
                format!(
                    " ({})",
                    self.created.iter().map(|(id, n)| format!("#{id} {n}")).collect::<Vec<_>>().join(", ")
                )
            },
            self.revised.len(),
            self.evidence,
            self.busy,
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
pub async fn live_start(ws: &Path, now: i64, persist: bool) -> Result<i64> {
    if let Some(v) = chore_state::watermark(ws, source::LIVE_START_KEY).await? {
        if let Ok(t) = v.trim().parse::<i64>() {
            return Ok(t);
        }
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

/// The new parts of a candidate, the position its reading ends at, and the
/// session's opening request.
async fn load(env: &Env<'_>, c: &Candidate, window: usize) -> Result<(Vec<Part>, Option<String>)> {
    match c.source {
        Source::Transcript => {
            let path = c.path.clone();
            let read = tokio::task::spawn_blocking(move || transcript_events(&path, 0)).await??;
            let end = read.complete_lines as i64;
            // Fewer lines than the ledger position: the file was rewritten.
            let from = if c.from > end { 0 } else { c.from };
            let opening = parts::opening_request(read.events.iter().filter_map(|e| match &e.kind {
                TranscriptEventKind::Text { role: TurnRole::User, text, meta } => Some((text.as_str(), *meta)),
                _ => None,
            }));
            Ok((parts::transcript_parts(&read.events, from, end, window), opening))
        }
        Source::Index => {
            let Some(index) = env.index else { return Ok((Vec::new(), None)) };
            let turns = session_index::session_turns(index, &c.session_id, 0).await?;
            let end = c.turn_count.unwrap_or(turns.len() as i64);
            let opening = parts::opening_request(turns.iter().filter(|t| t.role == TurnRole::User).map(|t| (t.text.as_str(), false)));
            Ok((parts::index_parts(&turns, c.from, end, window), opening))
        }
    }
}

/// Run one pass. Returns its report; a review that could not reach its
/// reviewer is reported in `error`, not as `Err`.
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
    let inv = source::inventory(&env.transcripts_dir, env.cfg, &ledger_rows, &labels, &index_sessions, live_start, env.now);
    report.busy = inv.busy.len();
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
    let write_mode = opts.write_mode && pass == Pass::Live;

    'sessions: for (i, c) in candidates.iter().enumerate() {
        let (session_parts, opening) = match load(env, c, window).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(session = %c.session_id, "skill sweep: cannot read the session: {e:#}");
                continue;
            }
        };
        if session_parts.is_empty() {
            continue;
        }
        report.sessions += 1;
        let count = session_parts.len();
        for (k, part) in session_parts.iter().enumerate() {
            let size = if k + 1 == count { c.size } else { None };
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
                    "review {} [{}..{}) {} — {reason}",
                    c.session_id,
                    part.from,
                    part.to,
                    c.agent.as_deref().unwrap_or("terminal")
                ));
                continue;
            }

            let subject = Subject {
                session_id: &c.session_id,
                source: c.source,
                agent: c.agent.as_deref(),
                part,
                part_number: k + 1,
                part_count: count,
                opening: opening.as_deref().filter(|_| part.from > 0),
                reason: &reason,
            };
            let run_id = ledger::start_run(
                env.pool,
                &RunStart { pass: pass.as_str(), session_id: &c.session_id, source: c.source, from_pos: part.from, to_pos: part.to },
            )
            .await?;
            let result = if write_mode {
                review::review_write(backend, &env.roots.personal, &(env.library_summary)(), &subject).await
            } else {
                let lib = Library::load(env.pool, env.roots, (env.library_summary)()).await?;
                review::review_suggest(backend, env.pool, env.ws, env.roots, &lib, &subject, pass.origin()).await
            };
            match result {
                Ok(o) => {
                    // Recorded first: the suggestions are stored, whatever
                    // the ledger writes below do.
                    report.created.extend(o.created.iter().map(|(id, _, n)| (*id, n.clone())));
                    report.revised.extend(o.revised.iter().map(|(_, new_id)| *new_id));
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
        async fn write_review(&self, _window: &str, prompt: &str) -> Result<String> {
            self.prompts.lock().unwrap().push(prompt.to_string());
            Ok("Nothing to save.".into())
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

    async fn pass(w: &World, index: Option<&SqlitePool>, pass: Pass, opts: PassOptions, fake: &Fake) -> PassReport {
        let s = summary;
        let env = Env {
            ws: &w.ws,
            cfg: &w.cfg,
            roots: &w.roots,
            pool: &w.pool,
            index,
            transcripts_dir: w.dir.clone(),
            library_summary: &s,
            now: w.now,
        };
        run_pass(&env, pass, opts, fake).await.unwrap()
    }

    fn opts(limit: u32) -> PassOptions {
        PassOptions { limit, dry_run: false, write_mode: false, daily_cap: true }
    }

    fn user(t: &str) -> String {
        json!({"type": "user", "message": {"role": "user", "content": t}}).to_string()
    }
    fn asst(t: &str) -> String {
        json!({"type": "assistant", "message": {"role": "assistant", "content": [{"type": "text", "text": t}]}}).to_string()
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
        let env = Env {
            ws: &w.ws,
            cfg: &w.cfg,
            roots: &w.roots,
            pool: &w.pool,
            index: None,
            transcripts_dir: w.dir.clone(),
            library_summary: &s,
            now: w.now,
        };
        let failed = run_pass(&env, Pass::Backfill, opts(5), &fake).await.unwrap_err();
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
        transcript(&w, "s-old", &[user("stop pushing before the tests pass"), asst("Understood.")], 2 * 3600);
        let fake2 = Fake::with(&[&reply(vec![json!({"action": "add_evidence", "suggestion_id": s.id, "note": "same correction again", "quotes": ["stop pushing before the tests pass"]})])]);
        let live = pass(&w, None, Pass::Live, opts(5), &fake2).await;
        assert_eq!((live.reviewed, live.evidence), (1, 1), "{}", live.summary());
        let p = &fake2.prompts()[0];
        assert!(p.contains("stop pushing before the tests pass"));
        assert!(!p.contains("USER: release the build"), "the reviewed part is not sent again");
        assert!(p.contains("opening request of the session (data)>>>\nUSER: release the build") || p.contains("opening request of the session (data)>>>\nrelease the build"), "{p}");
        assert!(p.contains(&format!("#{} create release-build", s.id)), "pending suggestions are shown");
        assert_eq!(suggestions::get(&w.pool, s.id).await.unwrap().unwrap().evidence.len(), 2);
        assert_eq!(ledger::all(&w.pool).await.unwrap()["s-old"].position, 6);
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
    async fn busy_skipped_filtered_and_dry_runs() {
        let w = world().await;
        std::fs::create_dir_all(w.ws.join("memory/logs/reminders-fire")).unwrap();
        std::fs::write(
            w.ws.join("memory/logs/reminders-fire/runs.jsonl"),
            format!("{}\n", json!({"session_id": "fire", "agent": "reminders-fire"})),
        )
        .unwrap();
        transcript(&w, "fire", &[user("run the report"), user("no, wrong"), asst("ok")], 3600);
        transcript(&w, "busy", &[user("hi"), user("no, wrong"), asst("ok")], 60);
        transcript(&w, "calm", &[user("what time is it"), asst("noon")], 3600);
        transcript(&w, "fix", &[user("rename it"), user("wrong file, the other one"), asst("ok")], 3600);

        let dry = pass(&w, None, Pass::Live, PassOptions { limit: 5, dry_run: true, write_mode: false, daily_cap: false }, &Fake::default()).await;
        assert_eq!((dry.busy, dry.skipped_agent, dry.parts_filtered, dry.reviewed), (1, 1, 1, 1), "{}", dry.summary());
        assert!(dry.lines.iter().any(|l| l.starts_with("review fix")));
        assert!(ledger::all(&w.pool).await.unwrap().is_empty(), "a dry run records nothing");

        let fake = Fake::with(&[r#"{"items": []}"#]);
        let r = pass(&w, None, Pass::Live, opts(5), &fake).await;
        assert_eq!((r.reviewed, r.parts_filtered), (1, 1));
        let rows = ledger::all(&w.pool).await.unwrap();
        assert_eq!(rows["fire"].outcome, "skipped-agent");
        assert_eq!(rows["calm"].outcome, "filtered");
        assert_eq!(rows["fix"].outcome, "reviewed");
        assert!(!rows.contains_key("busy"));
        // Write mode goes through the write path.
        transcript(&w, "fix", &[user("no, you forgot the header"), asst("added")], 3600);
        let fake = Fake::default();
        let r = pass(&w, None, Pass::Live, PassOptions { limit: 5, dry_run: false, write_mode: true, daily_cap: false }, &fake).await;
        assert_eq!(r.reviewed, 1, "{}", r.summary());
        assert!(fake.prompts()[0].contains("Write skills with your own Read/Write/Edit file tools"));
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
