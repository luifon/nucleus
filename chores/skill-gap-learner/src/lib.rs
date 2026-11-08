//! skill-gap-learner — the all-facets successor to preference-learner
//! (ADR-017, ADR-038).
//!
//! One nightly run (`learn`, launchd-cron) with these steps, in order:
//!   1. live sweep — review the completed turns of this workspace's session
//!      transcripts written since the last run (ADR-038).
//!   2. backfill — the same review over old transcripts and over sessions
//!      whose transcript only survives in the session index, a limited
//!      batch per night.
//!   3. candidate workflows — expire the stale ones and promote the ones
//!      seen on enough different days ([`promote`]).
//!   4. the pure, no-model archive of stale agent-created skills.
//!   5. gap detection over the agent diaries, and the curator (ADR-017's
//!      periodic arm). The diary window stretches back over missed runs
//!      (ADR-029 watermark).
//!
//! Every model pass files skill suggestions for the operator; none writes a
//! skill. The operator accepts or rejects them (`nucleus skills
//! suggestions`, the dashboard). At the end of the run, when any pass filed
//! or revised suggestions that are still pending, one WhatsApp notice goes
//! to the operator's DM with their count and the dashboard link
//! ([`notice`]).
//!
//! `sweep`, `backfill` and `promote` run steps 1, 2 and 3 alone, with
//! `--limit` and `--dry-run` for a manual check (a dry run works on a
//! private copy of the store and writes nothing). `backlog` lists the
//! sessions whose completed turns have waited too long for review, for the
//! heartbeat; it writes nothing.

mod notice;
mod parts;
mod prefilter;
mod promote;
mod review;
mod source;
mod sweep;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use clap::{Parser, Subcommand};
use nucleus_core::{
    chore_state,
    claude_session::{AskOptions, Session},
    config::Settings,
    diary, skill_learner,
    session_profile::{ProfileContext, SessionProfile},
    skills,
};
use std::path::Path;
use std::time::Duration;

const AGENT_NAME: &str = "skill-gap-learner";
const TMUX_SESSION: &str = "nucleus-skill-gap-learner";
/// Held for the whole run, so two runs never review the same session or
/// kill each other's tmux session.
const LOCK_PATH: &str = "memory/skill-gap-learner.lock";
/// Ceiling for one review answer: a review drafts whole SKILL.md files.
const REVIEW_MAX_WAIT: Duration = Duration::from_secs(300);

/// Watermark key for the periodic arm: the local date of the last `learn`
/// that completed (ADR-029). The gap pass reads at least
/// [`GAP_WINDOW_DAYS`] of diary, and further back when runs were missed.
const LEARN_WATERMARK_KEY: &str = "skill-gap-learner.learn";
/// The minimum diary window the gap pass reasons over.
const GAP_WINDOW_DAYS: i64 = 7;
/// Most bytes of one agent's diary of one day in the gap prompt.
const DIARY_ENTRY_BYTES: usize = 4 * 1024;

#[derive(Parser)]
#[command(name = "skill-gap-learner", about = "Nucleus skill-gap learner (ADR-017, ADR-038)")]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// The nightly run: live sweep, backfill batch, candidate promotion,
    /// then gap detection and curation. (default)
    Learn,
    /// Review the completed turns written since the last sweep.
    Sweep {
        /// Most review sessions (default: `[skill_learner] live_max_reviews`).
        #[arg(long)]
        limit: Option<u32>,
        /// Show which parts would be reviewed; review and record nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Review one batch of old sessions.
    Backfill {
        /// Most review sessions (default: `[skill_learner] backfill_per_night`).
        #[arg(long)]
        limit: Option<u32>,
        /// Show which parts would be reviewed; review and record nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// Expire stale candidate workflows and promote the ready ones.
    Promote {
        /// Most promotions (default: `[skill_learner] promotions_per_night`).
        #[arg(long)]
        limit: Option<u32>,
        /// Show what would be expired and promoted; change nothing.
        #[arg(long)]
        dry_run: bool,
    },
    /// List the sessions the sweep reviews whose completed turns have
    /// waited longer than `--hours` for review. Prints nothing when none
    /// do. Read-only.
    Backlog {
        #[arg(long, default_value_t = 48)]
        hours: u32,
    },
    /// Clear a session's review record so the next pass reviews it again
    /// from its start.
    Forget {
        /// Session id (the transcript's file name without `.jsonl`).
        #[arg(long = "session", required = true)]
        sessions: Vec<String>,
    },
}

/// The run lock (`flock` on [`LOCK_PATH`], released when the process ends).
struct RunLock {
    _file: std::fs::File,
}

fn take_lock(workspace_root: &Path) -> Result<RunLock> {
    let path = workspace_root.join(LOCK_PATH);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(RunLock { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("another skill-gap-learner run holds {}", path.display())
        }
        Err(std::fs::TryLockError::Error(e)) => Err(e).with_context(|| format!("locking {}", path.display())),
    }
}

/// Entry point for this subcommand of the `nucleus` binary. `args` is the
/// full argv for the subcommand, argv[0] included, so clap renders usage
/// under the right name.
pub async fn run(args: Vec<std::ffi::OsString>) -> Result<()> {
    nucleus_core::init_tracing();
    let settings = Settings::load().context("loading settings")?;
    let workspace_root = settings.workspace_root()?;
    let cli = Cli::parse_from(args);
    let command = cli.command.unwrap_or(Cmd::Learn);
    match &command {
        Cmd::Forget { sessions } => {
            let pool = skill_learner::open(&workspace_root).await?;
            for id in sessions {
                if skill_learner::ledger::forget(&pool, id).await? {
                    println!("forgot {id}: the next pass reviews it from the start");
                } else {
                    println!("{id}: no review record");
                }
            }
            return Ok(());
        }
        Cmd::Backlog { hours } => return backlog(&workspace_root, &settings, *hours).await,
        _ => {}
    }
    let dry_run = matches!(
        command,
        Cmd::Sweep { dry_run: true, .. } | Cmd::Backfill { dry_run: true, .. } | Cmd::Promote { dry_run: true, .. }
    );

    let _lock = if dry_run { None } else { Some(take_lock(&workspace_root)?) };
    if !dry_run {
        // A stale tmux session left from a prior crash blocks `new-window`.
        let _ = tokio::process::Command::new("tmux")
            .args(["kill-session", "-t", TMUX_SESSION])
            .output()
            .await;
    }

    let cfg = &settings.skill_learner;
    match command {
        Cmd::Learn => learn(&workspace_root, &settings).await,
        Cmd::Sweep { limit, dry_run } => {
            let opts = sweep::PassOptions { limit: limit.unwrap_or(cfg.live_max_reviews), dry_run, daily_cap: false };
            let report = run_one_pass(&workspace_root, &settings, sweep::Pass::Live, opts).await?;
            print_report(&report);
            report_result(&report)
        }
        Cmd::Backfill { limit, dry_run } => {
            let opts = sweep::PassOptions { limit: limit.unwrap_or(cfg.backfill_per_night), dry_run, daily_cap: false };
            let report = run_one_pass(&workspace_root, &settings, sweep::Pass::Backfill, opts).await?;
            print_report(&report);
            report_result(&report)
        }
        Cmd::Promote { limit, dry_run } => {
            let report = run_promote(&workspace_root, &settings, limit.unwrap_or(cfg.promotions_per_night), dry_run).await?;
            for l in &report.lines {
                println!("{l}");
            }
            println!("{}", report.summary());
            match &report.error {
                Some(e) => bail!("promotion stopped: {e}"),
                None => Ok(()),
            }
        }
        Cmd::Forget { .. } | Cmd::Backlog { .. } => unreachable!("handled before the run lock"),
    }
}

fn print_report(r: &sweep::PassReport) {
    for l in &r.lines {
        println!("{l}");
    }
    println!("{}", r.summary());
}

fn report_result(r: &sweep::PassReport) -> Result<()> {
    match &r.error {
        Some(e) => bail!("{} pass stopped: {e}", r.pass),
        None => Ok(()),
    }
}

/// The production [`review::Backend`]: Claude sessions in the learner's
/// tmux session.
struct ClaudeBackend<'a> {
    workspace_root: &'a Path,
    settings: &'a Settings,
}

struct ClaudeReview {
    session: Option<Session>,
    ask: AskOptions,
}

#[async_trait]
impl review::ReviewSession for ClaudeReview {
    async fn ask(&mut self, prompt: &str) -> Result<String> {
        let session = self.session.as_mut().context("the review session is closed")?;
        session.ask(prompt, self.ask.clone()).await
    }

    fn session_id(&self) -> Option<String> {
        self.session.as_ref().map(|s| s.session_id().to_string())
    }

    async fn close(mut self: Box<Self>) {
        if let Some(s) = self.session.take() {
            let _ = s.close().await;
        }
    }
}

#[async_trait]
impl review::Backend for ClaudeBackend<'_> {
    /// A fresh no-tools session per review, not the day's session (ADR-029):
    /// each review prompt is self-contained and up to ~60 KB, so resuming
    /// one session would carry every earlier review into the next and end
    /// in auto-compaction. Each review session is labelled with this agent
    /// and recorded in the ledger as the learner's own, so no sweep reviews
    /// it.
    async fn open(&self, window: &str) -> Result<Box<dyn review::ReviewSession>> {
        let (session, ask) = SessionProfile::one_shot_no_tools(&ProfileContext {
            workspace_root: self.workspace_root,
            claude: &self.settings.claude,
            tmux_session: TMUX_SESSION,
            agent_label: AGENT_NAME,
        })
        .window_name(window)
        .max_wait(REVIEW_MAX_WAIT)
        .spawn()
        .await?;
        Ok(Box::new(ClaudeReview { session: Some(session), ask }))
    }
}

/// The store a command works on: the real one, or for a dry run a private
/// copy (`skill_learner::open_snapshot`), so a dry run never writes or
/// migrates the real file. The guard keeps the copy alive.
async fn open_store(workspace_root: &Path, dry_run: bool) -> Result<(Option<tempfile::TempDir>, sqlx::SqlitePool)> {
    if dry_run {
        let (dir, pool) = skill_learner::open_snapshot(workspace_root).await?;
        Ok((Some(dir), pool))
    } else {
        Ok((None, skill_learner::open(workspace_root).await?))
    }
}

/// The library summary closure every review prompt uses.
fn summary_fn(workspace_root: &Path) -> impl Fn() -> String + Sync + use<> {
    let operator_root = skills::personal_skills_root(workspace_root);
    let repo_root = skills::repo_skills_root(workspace_root);
    move || library_summary(&operator_root, &repo_root, skills::global_skills_root().as_deref())
}

/// Run one pass with the production backend.
async fn run_one_pass(
    workspace_root: &Path,
    settings: &Settings,
    pass: sweep::Pass,
    opts: sweep::PassOptions,
) -> Result<sweep::PassReport> {
    let (_copy, pool) = open_store(workspace_root, opts.dry_run).await?;
    let index = nucleus_core::session_index::open_read_only(workspace_root).await?;
    let roots = skills::LibraryRoots::for_workspace(workspace_root);
    let summary = summary_fn(workspace_root);
    let env = sweep::Env {
        ws: workspace_root,
        cfg: &settings.skill_learner,
        roots: &roots,
        pool: &pool,
        index: index.as_ref(),
        transcripts_dir: nucleus_core::session_index::transcripts_dir(workspace_root),
        library_summary: &summary,
        now: chrono::Utc::now().timestamp(),
    };
    let backend = ClaudeBackend { workspace_root, settings };
    // A `PassFailed` keeps the report of what the pass did before its error.
    sweep::run_pass(&env, pass, opts, &backend).await.map_err(anyhow::Error::new)
}

/// Expire and promote candidate workflows with the production backend.
async fn run_promote(workspace_root: &Path, settings: &Settings, limit: u32, dry_run: bool) -> Result<promote::PromoteReport> {
    let (_copy, pool) = open_store(workspace_root, dry_run).await?;
    let index = nucleus_core::session_index::open_read_only(workspace_root).await?;
    let roots = skills::LibraryRoots::for_workspace(workspace_root);
    let summary = summary_fn(workspace_root);
    let env = sweep::Env {
        ws: workspace_root,
        cfg: &settings.skill_learner,
        roots: &roots,
        pool: &pool,
        index: index.as_ref(),
        transcripts_dir: nucleus_core::session_index::transcripts_dir(workspace_root),
        library_summary: &summary,
        now: chrono::Utc::now().timestamp(),
    };
    let backend = ClaudeBackend { workspace_root, settings };
    promote::run(&env, limit, dry_run, &backend).await
}

/// `backlog`: the sessions whose oldest completed turn waiting for review
/// ended more than `hours` ago, one line each. Opens every database
/// read-only and writes nothing.
async fn backlog(workspace_root: &Path, settings: &Settings, hours: u32) -> Result<()> {
    let (_copy, pool) = match skill_learner::open_read_only(workspace_root).await? {
        Some(p) => (None, p),
        // No store yet: an empty private copy, so every turn counts.
        None => {
            let (dir, p) = skill_learner::open_snapshot(workspace_root).await?;
            (Some(dir), p)
        }
    };
    let roots = skills::LibraryRoots::for_workspace(workspace_root);
    let summary = || String::new();
    let now = chrono::Utc::now().timestamp();
    let env = sweep::Env {
        ws: workspace_root,
        cfg: &settings.skill_learner,
        roots: &roots,
        pool: &pool,
        index: None,
        transcripts_dir: nucleus_core::session_index::transcripts_dir(workspace_root),
        library_summary: &summary,
        now,
    };
    let tz = nucleus_core::claude_session::nucleus_tz();
    for w in sweep::waiting(&env).await? {
        if now - w.oldest < i64::from(hours) * 3600 {
            continue;
        }
        let at = chrono::DateTime::from_timestamp(w.oldest, 0)
            .map(|t| t.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
        println!(
            "{} {} oldest waiting turn ended {at}, {} completed turn(s) waiting",
            w.session_id.get(..8).unwrap_or(&w.session_id),
            w.agent.as_deref().unwrap_or("terminal"),
            w.turns
        );
    }
    Ok(())
}

/// What the nightly run's passes did: their summary lines, the first error,
/// and every suggestion they filed or revised (the notice counts them).
#[derive(Default)]
struct Nightly {
    lines: Vec<String>,
    error: Option<String>,
    filed: Vec<i64>,
}

impl Nightly {
    fn fail(&mut self, msg: String) {
        tracing::warn!("learn: {msg}");
        self.lines.push(msg.clone());
        self.error.get_or_insert(msg);
    }
}

/// Steps 1 and 2 of the nightly run.
async fn nightly_sweep(workspace_root: &Path, settings: &Settings, out: &mut Nightly) {
    let cfg = &settings.skill_learner;
    let passes = [
        (sweep::Pass::Live, sweep::PassOptions { limit: cfg.live_max_reviews, dry_run: false, daily_cap: false }),
        (sweep::Pass::Backfill, sweep::PassOptions { limit: cfg.backfill_per_night, dry_run: false, daily_cap: true }),
    ];
    for (pass, opts) in passes {
        match run_one_pass(workspace_root, settings, pass, opts).await {
            Ok(r) => {
                tracing::info!("learn: {}", r.summary());
                out.filed.extend(r.created.iter().map(|(id, _)| *id));
                out.filed.extend(&r.revised);
                out.lines.push(r.summary());
                if let Some(e) = &r.error {
                    out.error.get_or_insert_with(|| e.clone());
                    // The reviewer is unreachable (usage limit, spawn
                    // failure): the next pass would fail the same way.
                    break;
                }
            }
            Err(e) => {
                // The suggestions filed before the error are stored: they
                // still get their notice.
                if let Some(failed) = e.downcast_ref::<sweep::PassFailed>() {
                    out.filed.extend(failed.report.created.iter().map(|(id, _)| *id));
                    out.filed.extend(&failed.report.revised);
                }
                out.fail(format!("{} pass failed: {e:#}", pass.as_str()));
            }
        }
    }
}

/// Queue the WhatsApp notice for the suggestions the run filed or revised
/// that are still pending (none when there are none). A failure is logged;
/// it does not stop the run.
async fn notify_suggestions(workspace_root: &Path, settings: &Settings, filed: &[i64]) {
    let cfg = &settings.skill_learner;
    if !cfg.notify || filed.is_empty() {
        return;
    }
    let result = async {
        let pool = skill_learner::open(workspace_root).await?;
        let pending = notice::new_pending(&pool, filed).await?;
        if pending.is_empty() {
            return Ok(None);
        }
        let store = skill_learner::store_id(&pool).await?;
        let wa = nucleus_core::whatsapp_queue::open(workspace_root).await?;
        notice::queue(&wa, &store, &cfg.texts, settings.public_urls.nucleus.as_deref(), &pending)
            .await
            .map(|row| row.map(|r| (r, pending.len())))
    }
    .await;
    match result {
        Ok(Some((row, n))) => tracing::info!("learn: queued the WhatsApp notice for {n} new suggestion(s) (outbound #{row})"),
        Ok(None) => {}
        Err(e) => tracing::warn!("learn: the WhatsApp notice for new suggestions was not queued: {e:#}"),
    }
}

fn render_library(lib: &[skills::Skill]) -> String {
    if lib.is_empty() {
        return "(the skill library is currently empty)".into();
    }
    let mut out = String::new();
    for s in lib {
        out.push_str(&format!("- {} [{}]: {}\n", s.name, s.tier, truncate(&s.description, 120)));
    }
    out
}

/// The library text every prompt shows: the personal + repo skills, then the
/// machine-wide skills as a separate read-only section. A machine-wide skill
/// loads in every project and wins over a same-named skill here, so the
/// model must neither recreate nor edit one. The section is omitted when
/// HOME or the machine-wide tree is missing or empty.
fn library_summary(operator_root: &Path, repo_root: &Path, machine_root: Option<&Path>) -> String {
    let mut lib = skills::read_skills(operator_root, skills::SkillTier::Personal);
    lib.extend(skills::read_skills(repo_root, skills::SkillTier::Repo));
    let mut out = render_library(&lib);
    let machine = machine_root.map(|r| skills::read_skills(r, skills::SkillTier::Global)).unwrap_or_default();
    if !machine.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(
            "\nMachine-wide skills (~/.claude/skills — READ-ONLY: they load in every session and shadow a same-named skill; do not recreate, patch, archive, or reuse these names):\n",
        );
        out.push_str(&render_library(&machine));
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
}

// ── nightly run: sweep, backfill, candidates, gap detection, curator ───────

/// One library-level review (the gap pass or the curator): a `review_runs`
/// row, one review session, its suggestions filed. Returns the summary line
/// and the suggestions filed or revised; a reviewer that cannot be reached
/// is an error.
async fn library_review(
    workspace_root: &Path,
    settings: &Settings,
    pass: &str,
    task: &review::Task<'_>,
    origin: skill_learner::suggestions::SuggestionOrigin,
) -> Result<(String, Vec<i64>)> {
    use skill_learner::ledger;
    let pool = skill_learner::open(workspace_root).await?;
    let roots = skills::LibraryRoots::for_workspace(workspace_root);
    let backend = ClaudeBackend { workspace_root, settings };
    let run_id = ledger::start_run(&pool, &ledger::RunStart { pass, ..ledger::RunStart::default() }).await?;
    let lib = review::Library::load(&pool, &roots, summary_fn(workspace_root)()).await?;
    match review::review(&backend, &pool, workspace_root, &roots, &lib, task, origin).await {
        Ok(o) => {
            // The suggestions are stored: they are returned (for the notice)
            // even when the run row cannot be closed.
            let mut filed: Vec<i64> = o.created.iter().map(|(id, _, _)| *id).collect();
            filed.extend(o.revised.iter().map(|(_, new)| *new));
            let detail = review::describe(&o);
            if let Err(e) = ledger::finish_run(
                &pool,
                run_id,
                "ok",
                o.review_session_id.as_deref(),
                o.created.len() as i64,
                o.evidence_for.len() as i64,
                Some(&detail),
            )
            .await
            {
                tracing::warn!("learn: {pass}: the review-run row was not closed: {e:#}");
            }
            Ok((format!("{pass}: {detail}"), filed))
        }
        Err(e) => {
            let outcome = if matches!(e, review::ReviewError::Infra(_)) { "failed" } else { "unparsable" };
            ledger::finish_run(&pool, run_id, outcome, None, 0, 0, Some(&e.to_string())).await?;
            match e {
                review::ReviewError::Infra(_) => bail!("{pass}: {e}"),
                _ => Ok((format!("{pass}: {e}"), Vec::new())),
            }
        }
    }
}

/// The nightly run (launchd-cron): the live sweep and a backfill batch
/// (ADR-038), the candidate workflows, then the ADR-017 periodic arm: pure
/// auto-archive of stale agent-created skills, the gap pass over recent
/// diaries and the curator, both as suggestions. A step that fails does not
/// stop the later ones; the run reports the first failure at the end, after
/// the one WhatsApp notice for everything the run filed.
async fn learn(workspace_root: &Path, settings: &Settings) -> Result<()> {
    let operator_root = skills::personal_skills_root(workspace_root);
    let cfg = &settings.skill_learner;
    let mut run = Nightly::default();

    // 1–3. Transcript sweep + backfill, then the candidate workflows.
    if cfg.enabled {
        nightly_sweep(workspace_root, settings, &mut run).await;
        if run.error.is_none() {
            match run_promote(workspace_root, settings, cfg.promotions_per_night, false).await {
                Ok(r) => {
                    tracing::info!("learn: {}", r.summary());
                    run.filed.extend(&r.filed);
                    run.lines.push(r.summary());
                    if let Some(e) = r.error {
                        run.error.get_or_insert(e);
                    }
                }
                Err(e) => run.fail(format!("candidate promotion failed: {e:#}")),
            }
        }
    } else {
        run.lines.push("sweep disabled ([skill_learner] enabled = false)".into());
    }
    let _ = diary::record_observation(workspace_root, AGENT_NAME, "sweep", &run.lines.join("\n"), diary::Tag::Observation);

    // 4. Pure auto-transitions (no model), under the library write lock so
    // an accept never writes a skill this moves.
    let (stale, archived) = match skill_learner::library_lock(workspace_root, Duration::from_secs(120)).await {
        Ok(_library) => apply_auto_transitions(&operator_root, cfg.stale_after_days, cfg.archive_after_days),
        Err(e) => {
            tracing::warn!("learn: stale-skill archive skipped: {e:#}");
            (0, Vec::new())
        }
    };
    if !archived.is_empty() {
        tracing::info!("learn: auto-archived {} stale skill(s): {}", archived.len(), archived.join(", "));
    }

    // 5a. Gap detection over recent diaries (skip our own). The window is
    // GAP_WINDOW_DAYS, stretched back to the day after the last completed
    // run when nights were missed (ADR-029 watermark).
    let today = chrono::Local::now().date_naive();
    let floor = today - chrono::Duration::days(GAP_WINDOW_DAYS - 1);
    // No `?` before the notice: the suggestions filed above get it whatever
    // fails here.
    let from = match chore_state::resume_date_after(workspace_root, LEARN_WATERMARK_KEY, GAP_WINDOW_DAYS - 1).await {
        Ok(d) => d.min(floor),
        Err(e) => {
            run.fail(format!("reading the gap-pass watermark failed: {e:#}"));
            floor
        }
    };
    let window_days = (today - from).num_days() + 1;
    if window_days > GAP_WINDOW_DAYS {
        tracing::info!("learn: catching up — gap window is {window_days} days (from {from})");
    }
    let diaries = read_all_diaries(workspace_root, &settings.diary.root, window_days, review::MAX_WINDOW_BYTES);
    let gap_summary = if diaries.text.trim().is_empty() {
        "gap: no diaries to scan".to_string()
    } else {
        let task = review::Task::Gap { diaries: &diaries.text, window: &diaries.window };
        match library_review(workspace_root, settings, "gap", &task, skill_learner::suggestions::SuggestionOrigin::Gap).await {
            Ok((line, filed)) => {
                run.filed.extend(filed);
                line
            }
            Err(e) => {
                run.fail(format!("gap pass failed: {e:#}"));
                "gap: failed".into()
            }
        }
    };

    // 5b. The curator over the agent-created, unpinned skills.
    let agent_skills: Vec<String> = skills::read_skills(&operator_root, skills::SkillTier::Personal)
        .into_iter()
        .filter(|s| s.created_by.as_deref() == Some("agent") && !s.pinned && s.symlink_target.is_none())
        .map(|s| s.dir_name)
        .collect();
    let curate_summary = if agent_skills.len() < 2 {
        format!("curate: {} agent-created skill(s), nothing to consolidate", agent_skills.len())
    } else {
        let task = review::Task::Curate { skills: &agent_skills };
        match library_review(workspace_root, settings, "curate", &task, skill_learner::suggestions::SuggestionOrigin::Curator)
            .await
        {
            Ok((line, filed)) => {
                run.filed.extend(filed);
                line
            }
            Err(e) => {
                run.fail(format!("curator failed: {e:#}"));
                "curate: failed".into()
            }
        }
    };

    // One notice for the whole run.
    notify_suggestions(workspace_root, settings, &run.filed).await;

    let summary = format!("stale={stale} archived={} · {gap_summary} · {curate_summary}", archived.len());
    let nothing_happened = archived.is_empty()
        && gap_summary.ends_with("nothing to suggest")
        && (curate_summary.ends_with("nothing to suggest") || curate_summary.ends_with("nothing to consolidate"));
    let _ = diary::record_observation(
        workspace_root,
        AGENT_NAME,
        "learn",
        &summary,
        if nothing_happened { diary::Tag::Routine } else { diary::Tag::Observation },
    );
    tracing::info!("learn: {summary}");
    // Today's diaries are still being written; mark yesterday so the next
    // run re-reads today in full (same rule as the distiller).
    if let Some(yesterday) = today.pred_opt() {
        chore_state::set_watermark(workspace_root, LEARN_WATERMARK_KEY, &yesterday.to_string()).await?;
    }
    match run.error {
        Some(e) => bail!("skill-gap-learner: {e}"),
        None => Ok(()),
    }
}

/// Archive agent-created, unpinned skills whose last activity is older than
/// `archive_days`; return (stale_count, archived_names). "Activity" = the
/// later of frontmatter `last_used` and the SKILL.md mtime. Hand-written
/// skills (created_by != "agent") and pinned skills are never auto-managed.
fn apply_auto_transitions(root: &Path, stale_days: u32, archive_days: u32) -> (usize, Vec<String>) {
    let mut stale = 0usize;
    let mut archived = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return (0, archived),
    };
    let now = chrono::Utc::now();
    for dirent in entries.flatten() {
        let dir = dirent.path();
        if !dir.is_dir() {
            continue;
        }
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        if name.starts_with('.') {
            continue;
        }
        let skill_md = dir.join(skills::SKILL_FILE);
        let Ok(content) = std::fs::read_to_string(&skill_md) else { continue };
        let fm = skills::parse_frontmatter(&content, &skill_md).unwrap_or_default();
        if fm.created_by.as_deref() != Some("agent") || fm.pinned {
            continue;
        }
        let age_days = skill_age_days(&fm, &skill_md, now);
        if age_days >= archive_days as i64 {
            let ts = now.format("%Y%m%dT%H%M%S");
            let dest = root.join(skills::ARCHIVE_DIR).join(format!("{name}-{ts}"));
            if let Some(p) = dest.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            if std::fs::rename(&dir, &dest).is_ok() {
                archived.push(name);
            }
        } else if age_days >= stale_days as i64 {
            stale += 1;
        }
    }
    (stale, archived)
}

/// Days since a skill's last activity: max(last_used, mtime).
fn skill_age_days(fm: &skills::Frontmatter, skill_md: &Path, now: chrono::DateTime<chrono::Utc>) -> i64 {
    let mut newest: Option<chrono::DateTime<chrono::Utc>> = None;
    if let Some(lu) = &fm.last_used {
        // accept YYYY-MM-DD or RFC3339
        if let Ok(d) = chrono::NaiveDate::parse_from_str(lu.trim(), "%Y-%m-%d") {
            newest = d.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc());
        } else if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(lu.trim()) {
            newest = Some(dt.with_timezone(&chrono::Utc));
        }
    }
    if let Ok(meta) = std::fs::metadata(skill_md) {
        if let Ok(modified) = meta.modified() {
            let dt: chrono::DateTime<chrono::Utc> = modified.into();
            newest = Some(newest.map_or(dt, |n| n.max(dt)));
        }
    }
    match newest {
        Some(n) => (now - n).num_days(),
        None => 0,
    }
}


/// The diaries the gap pass reads, and the window they cover.
struct Diaries {
    text: String,
    /// `YYYY-MM-DD..YYYY-MM-DD` of the days included, with a note when the
    /// budget left older days out.
    window: String,
}

/// The last `days` of every agent's diary (skipping our own and hidden
/// files, routine entries removed), newest day first across agents, each
/// agent's day cut to [`DIARY_ENTRY_BYTES`], at most `budget` bytes: the
/// oldest days are left out first.
fn read_all_diaries(workspace_root: &Path, diary_root_rel: &str, days: i64, budget: usize) -> Diaries {
    let root = workspace_root.join(diary_root_rel);
    let today = chrono::Local::now().date_naive();
    let mut agents: Vec<(String, std::path::PathBuf)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&root) {
        for dirent in entries.flatten() {
            let dir = dirent.path();
            let agent = dir.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
            if !dir.is_dir() || agent == AGENT_NAME || agent.starts_with('.') {
                continue; // never learn from our own diary
            }
            agents.push((agent, dir));
        }
    }
    agents.sort();
    let mut out = String::new();
    let mut first: Option<chrono::NaiveDate> = None;
    let mut left_out = 0usize;
    for back in 0..days {
        let Some(date) = today.checked_sub_signed(chrono::Duration::days(back)) else { continue };
        for (agent, dir) in &agents {
            let Ok(c) = std::fs::read_to_string(dir.join(format!("{date}.md"))) else { continue };
            let kept = diary::without_routine(&c);
            if kept.trim().is_empty() {
                continue;
            }
            let block = format!("### {date} · agent: {agent}\n{}\n", review::budgeted(kept.trim(), DIARY_ENTRY_BYTES));
            if out.len() + block.len() > budget {
                left_out += 1;
                continue;
            }
            out.push_str(&block);
            first = Some(date);
        }
    }
    let from = first.unwrap_or(today);
    let mut window = format!("{from}..{today}");
    if left_out > 0 {
        window.push_str(&format!(", {left_out} older agent-day(s) left out for length"));
    }
    Diaries { text: out, window }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn write_skill(root: &Path, name: &str, frontmatter: &str, backdate: bool) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let md = dir.join("SKILL.md");
        std::fs::write(
            &md,
            format!("---\nname: {name}\ndescription: d\nflavor: learned\n{frontmatter}\n---\n\n# When to invoke\nx\n# Steps\n1\n# Failure modes\n- z\n"),
        )
        .unwrap();
        if backdate {
            // mtime is part of the activity anchor — backdate it well past
            // archive_after_days so the age test is meaningful.
            let _ = std::process::Command::new("touch")
                .args(["-t", "202001010000", md.to_str().unwrap()])
                .status();
        }
    }

    #[test]
    fn auto_transitions_archive_old_agent_skills_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

        // old + agent + unpinned → archived
        write_skill(&root, "old-agent", "created_by: agent\nlast_used: 2020-01-01", true);
        // old + agent + pinned → skipped
        write_skill(&root, "pinned-agent", "created_by: agent\npinned: true\nlast_used: 2020-01-01", true);
        // old + hand-written (no created_by) → skipped
        write_skill(&root, "hand-written", "last_used: 2020-01-01", true);
        // fresh agent → skipped (not old)
        write_skill(&root, "fresh-agent", "created_by: agent", false);

        let (_stale, archived) = apply_auto_transitions(&root, 30, 90);

        assert_eq!(archived, vec!["old-agent".to_string()], "only the old unpinned agent skill archives");
        assert!(root.join(".archive").exists(), "archive dir created");
        assert!(!root.join("old-agent").exists(), "archived skill moved out");
        assert!(root.join("pinned-agent").exists(), "pinned skill stays");
        assert!(root.join("hand-written").exists(), "hand-written skill stays");
        assert!(root.join("fresh-agent").exists(), "fresh skill stays");
    }

    /// Workspace with the personal + repo trees and a separate machine-wide
    /// tree, each holding one pre-existing (backdated) skill.
    struct Trees {
        _tmp: tempfile::TempDir,
        personal: PathBuf,
        repo: PathBuf,
        machine: PathBuf,
    }

    fn trees() -> Trees {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let personal = skills::personal_skills_root(&ws);
        let repo = skills::repo_skills_root(&ws);
        let machine = tmp.path().join("home/.claude/skills");
        write_skill(&repo, "committed-one", "", true);
        write_skill(&machine, "machine-one", "", true);
        write_skill(&personal, "old-personal", "created_by: agent", true);
        Trees { _tmp: tmp, personal, repo, machine }
    }

    #[test]
    fn library_summary_lists_machine_skills_as_a_read_only_section() {
        let t = trees();
        let with = library_summary(&t.personal, &t.repo, Some(&t.machine));
        let (main, machine) = with.split_once("Machine-wide skills").expect("machine section present");
        assert!(main.contains("- old-personal [personal]: d"));
        assert!(main.contains("- committed-one [repo]: d"));
        assert!(!main.contains("machine-one"));
        assert!(machine.contains("READ-ONLY"));
        assert!(machine.contains("- machine-one [global]: d"));

        let without = library_summary(&t.personal, &t.repo, None);
        assert!(!without.contains("Machine-wide"));
        let missing = t.machine.join("does-not-exist");
        assert_eq!(library_summary(&t.personal, &t.repo, Some(&missing)), without);
    }

    #[test]
    fn diaries_are_read_newest_day_first_within_the_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        let today = chrono::Local::now().date_naive();
        for (agent, back, text) in [("alpha", 0, "ALPHA-TODAY"), ("beta", 0, "BETA-TODAY"), ("alpha", 3, "ALPHA-OLD"), (AGENT_NAME, 0, "OWN-DIARY")] {
            let dir = ws.join("diary").join(agent);
            std::fs::create_dir_all(&dir).unwrap();
            let date = today - chrono::Duration::days(back);
            std::fs::write(dir.join(format!("{date}.md")), format!("## 10:00 — run\n- OBSERVATION: {text} {}\n", "x".repeat(300))).unwrap();
        }
        let all = read_all_diaries(ws, "diary", 7, 64 * 1024);
        assert!(all.text.find("ALPHA-TODAY").unwrap() < all.text.find("ALPHA-OLD").unwrap());
        assert!(all.text.contains("BETA-TODAY") && !all.text.contains("OWN-DIARY"));
        assert_eq!(all.window, format!("{}..{today}", today - chrono::Duration::days(3)));
        // A small budget keeps today and leaves the old day out.
        let cut = read_all_diaries(ws, "diary", 7, 800);
        assert!(cut.text.contains("ALPHA-TODAY") && !cut.text.contains("ALPHA-OLD"), "{}", cut.text);
        assert!(cut.window.contains("left out for length"), "{}", cut.window);
    }
}
