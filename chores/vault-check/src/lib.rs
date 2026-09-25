//! `vault-check` — the weekly vault check (ADR-035).
//!
//! A deterministic pass over the Obsidian vault: no Claude session. The
//! analysis, the safe fix and the run history live in
//! `nucleus_core::vault::check`; this crate is the command, the weekly
//! schedule gate, and the WhatsApp summary.
//!
//! Examples:
//!   nucleus vault-check                  # report only, recorded in history
//!   nucleus vault-check --apply          # also apply the safe fix
//!   nucleus vault-check --json           # full report as JSON
//!   nucleus vault-check --notify         # also enqueue the WhatsApp summary
//!   nucleus vault-check --scheduled      # what launchd runs hourly
//!
//! Schedule: launchd starts `--scheduled` every hour. The run proceeds only
//! when the `[vault_check] cron` expression (default Sunday 20:00, in
//! `NUCLEUS_TZ`) has had a match since the last scheduled run. That makes
//! the day and time a nucleus.toml setting, and a laptop that was closed at
//! the scheduled time runs the check at the next wake (the reminders
//! fire-late policy). The last scheduled run is an ADR-029 watermark.
//!
//! Each cron match is one occurrence, claimed in `vault_check.db` before
//! the check runs (`scheduled_claims`, keyed by the match time). Two
//! scheduled processes at once: one runs, the other exits. The WhatsApp
//! summary is enqueued with the occurrence as its idempotency key, so a
//! process that dies after enqueueing and before completing the claim does
//! not send a second summary when the claim is taken over. The claim is
//! marked complete after the summary is queued.
//!
//! Output: a manual run prints the findings with their paths. A scheduled
//! run (its stdout is the launchd log) prints counts and fix outcomes only,
//! unless `--details` is given.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use nucleus_core::config::Settings;
use nucleus_core::vault::check::{self, CheckOptions, CheckReport, Claim};
use nucleus_core::vault::exclude::Exclusions;
use std::path::{Path, PathBuf};

const WATERMARK_KEY: &str = "vault-check.scheduled";
/// Dashboard route that shows the latest report.
const DASHBOARD_ROUTE: &str = "/vault/check";
/// A claim older than this with no completion belongs to a process that
/// died; the next hourly wake takes it over.
const STALE_CLAIM_MINUTES: i64 = 30;

#[derive(Parser)]
#[command(
    name = "vault-check",
    about = "Structural check of the Obsidian vault (ADR-035): duplicates, broken links, orphans, stale inbox, frontmatter, sources, empty files"
)]
struct Cli {
    /// Apply the safe fix: move empty untitled files nothing links to into
    /// the quarantine. Default: report only. Notes are never rewritten.
    #[arg(long)]
    apply: bool,
    /// Print the full report as JSON.
    #[arg(long)]
    json: bool,
    /// Enqueue the one-line summary to the operator's WhatsApp DM.
    #[arg(long)]
    notify: bool,
    /// Weekly mode: run only when `[vault_check] cron` is due; apply and
    /// notify follow `scheduled_apply` / `notify` in nucleus.toml.
    #[arg(long, conflicts_with_all = ["apply", "notify"])]
    scheduled: bool,
    /// With --scheduled, print findings with their paths (they otherwise
    /// stay out of the launchd log).
    #[arg(long, requires = "scheduled")]
    details: bool,
    /// Do not store this run in the history.
    #[arg(long)]
    no_record: bool,
    /// Findings printed per kind in the text report (0 = all).
    #[arg(long, default_value_t = 15)]
    show: usize,
}

pub async fn run(args: Vec<std::ffi::OsString>) -> Result<()> {
    nucleus_core::init_tracing();
    let cli = Cli::parse_from(args);
    let settings = Settings::load().context("loading settings")?;
    let workspace_root = settings.workspace_root()?;
    let cfg = &settings.vault_check;
    let vault = settings.obsidian.vault_dir();
    let ex = Exclusions::load(&workspace_root)?;
    let link = settings
        .public_urls
        .nucleus
        .as_deref()
        .map(|u| format!("{}{DASHBOARD_ROUTE}", u.trim_end_matches('/')));

    if cli.scheduled {
        let mut opts = CheckOptions::from_config(cfg, cfg.scheduled_apply, "scheduled")?;
        opts.quarantine_dir = Some(workspace_root.join(check::QUARANTINE_DIR));
        let ctx = Scheduled {
            workspace_root: workspace_root.clone(),
            cron: cfg.cron.clone(),
            notify_target: if cfg.notify { reminders::operator_whatsapp_dm() } else { None },
            notify: cfg.notify,
            link,
            record: !cli.no_record,
        };
        let outcome = run_scheduled(&ctx, Utc::now(), move || check::run(&vault, &ex, &opts)).await?;
        if let ScheduledOutcome::Ran(report) = outcome {
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if cli.details {
                print_report(&report, cli.show);
            } else {
                print_counts(&report);
            }
            let _ = nucleus_core::diary::record_observation(
                &workspace_root,
                "vault-check",
                "weekly",
                &check::summary_line(&report.counts, None),
                nucleus_core::diary::Tag::Routine,
            );
        }
        return Ok(());
    }

    let mut opts = CheckOptions::from_config(cfg, cli.apply, "manual")?;
    opts.quarantine_dir = Some(workspace_root.join(check::QUARANTINE_DIR));
    let mut report = tokio::task::spawn_blocking(move || check::run(&vault, &ex, &opts))
        .await
        .context("vault check task")??;
    if !cli.no_record {
        let pool = check::open(&workspace_root).await?;
        report.run_id = Some(check::record(&pool, &report).await?);
    }
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report, cli.show);
    }
    if cli.notify {
        let line = check::summary_line(&report.counts, link.as_deref());
        match reminders::operator_whatsapp_dm() {
            Some(target) => {
                let pool = nucleus_core::whatsapp_queue::open(&workspace_root).await?;
                nucleus_core::whatsapp_queue::enqueue_text(&pool, &target, &line, "vault-check").await?;
            }
            None => tracing::warn!("vault-check: WHATSAPP_ALLOWED_DM_JIDS is empty; summary not sent"),
        }
    }
    Ok(())
}

/// Inputs of one scheduled wake.
struct Scheduled {
    workspace_root: PathBuf,
    cron: String,
    notify: bool,
    notify_target: Option<String>,
    link: Option<String>,
    record: bool,
}

#[derive(Debug)]
enum ScheduledOutcome {
    /// Not due, or the schedule was just armed.
    NotDue,
    /// Another process runs this occurrence.
    Busy,
    /// A previous process already completed this occurrence.
    AlreadyDone,
    Ran(CheckReport),
}

/// One scheduled wake: gate on the cron, claim the occurrence, run the
/// check, record it, enqueue the summary once, complete the claim, advance
/// the watermark.
async fn run_scheduled(
    ctx: &Scheduled,
    now: DateTime<Utc>,
    check_fn: impl FnOnce() -> Result<CheckReport> + Send + 'static,
) -> Result<ScheduledOutcome> {
    let Some(occurrence) = due_occurrence(&ctx.workspace_root, &ctx.cron, now).await? else {
        return Ok(ScheduledOutcome::NotDue);
    };
    let key = occurrence.to_rfc3339();
    let pool = check::open(&ctx.workspace_root).await?;
    match check::claim_occurrence(&pool, &key, now, chrono::Duration::minutes(STALE_CLAIM_MINUTES)).await? {
        Claim::Acquired => {}
        Claim::Busy => return Ok(ScheduledOutcome::Busy),
        Claim::Completed => {
            // Completed, but the watermark did not advance (the process
            // stopped in between): advance it now.
            set_watermark(&ctx.workspace_root, now).await?;
            return Ok(ScheduledOutcome::AlreadyDone);
        }
    }

    let mut report = tokio::task::spawn_blocking(check_fn).await.context("vault check task")??;
    if ctx.record {
        report.run_id = Some(check::record(&pool, &report).await?);
    }
    if ctx.notify {
        match &ctx.notify_target {
            Some(target) => {
                let line = check::summary_line(&report.counts, ctx.link.as_deref());
                let wa = nucleus_core::whatsapp_queue::open(&ctx.workspace_root).await?;
                let source = format!("vault-check:{key}");
                let queued = reminders::store::enqueue_whatsapp_once(&wa, target, &line, &source).await?;
                tracing::info!(queued = queued.is_some(), "vault-check: summary for this occurrence queued");
            }
            None => tracing::warn!("vault-check: WHATSAPP_ALLOWED_DM_JIDS is empty; summary not sent"),
        }
    }
    check::complete_occurrence(&pool, &key, report.run_id, Utc::now()).await?;
    set_watermark(&ctx.workspace_root, now).await?;
    Ok(ScheduledOutcome::Ran(report))
}

async fn set_watermark(workspace_root: &Path, now: DateTime<Utc>) -> Result<()> {
    nucleus_core::chore_state::set_watermark(workspace_root, WATERMARK_KEY, &now.to_rfc3339()).await
}

/// The cron match a scheduled wake should run, if any. The first scheduled
/// wake after install only records the watermark, so installing the job
/// does not send a report immediately.
async fn due_occurrence(workspace_root: &Path, cron: &str, now: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
    let wm = nucleus_core::chore_state::watermark(workspace_root, WATERMARK_KEY).await?;
    let Some(last) = wm.and_then(|s| DateTime::parse_from_rfc3339(&s).ok()) else {
        set_watermark(workspace_root, now).await?;
        tracing::info!("vault-check: schedule armed; first run at the next `{cron}` match");
        return Ok(None);
    };
    let next = next_match(cron, last.with_timezone(&Utc))?;
    Ok((next <= now).then_some(next))
}

/// The first cron match after `last`.
fn next_match(cron: &str, last: DateTime<Utc>) -> Result<DateTime<Utc>> {
    reminders::store::next_match_utc(cron, last, reminders::store::nucleus_tz())
        .with_context(|| format!("[vault_check] cron {cron:?}"))
}

/// Due when the cron has a match after `last` and at or before `now`.
#[cfg(test)]
fn is_due(cron: &str, last: DateTime<Utc>, now: DateTime<Utc>) -> Result<bool> {
    Ok(next_match(cron, last)? <= now)
}

/// Counts and fix outcomes only: what a scheduled run writes to its log.
fn print_counts(r: &CheckReport) {
    let c = &r.counts;
    println!(
        "vault check ({}{}): {} notes, {} excluded, {} ms{}",
        r.trigger,
        if r.applied { ", fixes applied" } else { ", report only" },
        r.notes_scanned,
        r.files_excluded,
        r.duration_ms,
        r.run_id.map(|id| format!(", run #{id}")).unwrap_or_default(),
    );
    println!(
        "  duplicates {} · broken links {} · orphans {} · stale inbox {} · frontmatter {} · unknown source {} · empty files {} · oversized {} · fixed {}",
        c.duplicates, c.broken_links, c.orphans, c.stale_inbox, c.missing_frontmatter, c.unknown_source, c.empty_files, c.oversized, c.fixed
    );
    let not_applied = r
        .findings
        .iter()
        .filter(|f| f.fix_action.as_deref().is_some_and(|a| a.starts_with("not applied")))
        .count();
    if not_applied > 0 {
        println!("  {not_applied} fixes not applied (the files changed or became linked); see the dashboard report");
    }
}

fn print_report(r: &CheckReport, show: usize) {
    print_counts(r);
    let kinds = [
        (check::KIND_DUPLICATE_NAME, "same file name"),
        (check::KIND_SIMILAR_TITLE, "similar titles"),
        (check::KIND_DATED_SERIES, "dated series"),
        (check::KIND_DUPLICATE_CONTENT, "same content"),
        (check::KIND_BROKEN_LINK, "broken links"),
        (check::KIND_ORPHAN, "orphans"),
        (check::KIND_STALE_INBOX, "stale inbox"),
        (check::KIND_FRONTMATTER, "frontmatter"),
        (check::KIND_UNKNOWN_SOURCE, "unknown source"),
        (check::KIND_EMPTY_FILE, "empty files"),
        (check::KIND_OVERSIZED, "oversized, not checked"),
    ];
    for (kind, label) in kinds {
        let items: Vec<_> = r.findings.iter().filter(|f| f.kind == kind).collect();
        if items.is_empty() {
            continue;
        }
        println!("\n{label} ({})", items.len());
        let limit = if show == 0 { items.len() } else { show };
        for f in items.iter().take(limit) {
            let fix = f.fix_action.as_deref().map(|a| format!("  [{a}]")).unwrap_or_default();
            match &f.path {
                Some(p) => println!("  {p} — {}{fix}", f.detail),
                None => println!("  {}{fix}", f.detail),
            }
            for rel in f.related.iter().take(if show == 0 { usize::MAX } else { 6 }) {
                println!("      {rel}");
            }
            if show != 0 && f.related.len() > 6 {
                println!("      … {} more", f.related.len() - 6);
            }
        }
        if items.len() > limit {
            println!("  … {} more (--show 0 lists all)", items.len() - limit);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn due_only_after_a_cron_match() {
        // Pin the zone the cron is evaluated in.
        unsafe { std::env::set_var("NUCLEUS_TZ", "UTC") };
        let cron = "0 20 * * 0"; // Sunday 20:00
        // 2026-09-20 is a Sunday.
        let last = Utc.with_ymd_and_hms(2026, 9, 20, 20, 5, 0).unwrap();
        let before = Utc.with_ymd_and_hms(2026, 9, 27, 19, 59, 0).unwrap();
        let at = Utc.with_ymd_and_hms(2026, 9, 27, 20, 0, 0).unwrap();
        let late = Utc.with_ymd_and_hms(2026, 9, 29, 8, 0, 0).unwrap();
        assert!(!is_due(cron, last, before).unwrap());
        assert!(is_due(cron, last, at).unwrap());
        assert!(is_due(cron, last, late).unwrap(), "a missed match runs at the next wake");
        assert!(is_due("not a cron", last, late).is_err());
    }

    fn ctx(ws: &Path) -> Scheduled {
        Scheduled {
            workspace_root: ws.to_path_buf(),
            cron: "0 20 * * 0".into(),
            notify: true,
            notify_target: Some("5511999999999@s.whatsapp.net".into()),
            link: None,
            record: true,
        }
    }

    fn fake_report() -> Result<CheckReport> {
        let tmp = tempfile::tempdir()?;
        let opts = CheckOptions::from_config(&Default::default(), false, "scheduled")?;
        check::run(tmp.path(), &Exclusions::new(&[], "")?, &opts)
    }

    async fn queued(ws: &Path) -> Vec<String> {
        let pool = nucleus_core::whatsapp_queue::open(ws).await.unwrap();
        sqlx::query_scalar("SELECT source FROM outbound_queue").fetch_all(&pool).await.unwrap()
    }

    /// Two `--scheduled` processes at the same due wake: one runs the
    /// check and queues one summary; the other exits. Later wakes before
    /// the next match do nothing.
    #[tokio::test]
    async fn concurrent_scheduled_runs_send_one_summary() {
        unsafe { std::env::set_var("NUCLEUS_TZ", "UTC") };
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("memory")).unwrap();
        let last = Utc.with_ymd_and_hms(2026, 9, 20, 20, 5, 0).unwrap();
        set_watermark(ws, last).await.unwrap();
        let now = Utc.with_ymd_and_hms(2026, 9, 27, 21, 0, 0).unwrap();

        let c = ctx(ws);
        let (a, b) = tokio::join!(run_scheduled(&c, now, fake_report), run_scheduled(&c, now, fake_report));
        let outcomes = [a.unwrap(), b.unwrap()];
        let ran = outcomes.iter().filter(|o| matches!(o, ScheduledOutcome::Ran(_))).count();
        assert_eq!(ran, 1, "{outcomes:?}");
        assert!(outcomes.iter().any(|o| matches!(o, ScheduledOutcome::Busy | ScheduledOutcome::AlreadyDone | ScheduledOutcome::NotDue)));
        assert_eq!(queued(ws).await, vec!["vault-check:2026-09-27T20:00:00+00:00".to_string()]);

        let again = run_scheduled(&c, now + chrono::Duration::hours(1), fake_report).await.unwrap();
        assert!(matches!(again, ScheduledOutcome::NotDue), "{again:?}");
        assert_eq!(queued(ws).await.len(), 1);
        let runs = check::runs(&check::open(ws).await.unwrap(), 10).await.unwrap();
        assert_eq!(runs.len(), 1);
    }

    /// A process that queued the summary and died before completing the
    /// claim: the takeover reruns the check but does not queue again.
    #[tokio::test]
    async fn takeover_after_a_crash_does_not_resend() {
        unsafe { std::env::set_var("NUCLEUS_TZ", "UTC") };
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("memory")).unwrap();
        set_watermark(ws, Utc.with_ymd_and_hms(2026, 9, 20, 20, 5, 0).unwrap()).await.unwrap();
        let now = Utc.with_ymd_and_hms(2026, 9, 27, 20, 30, 0).unwrap();
        let key = "2026-09-27T20:00:00+00:00";

        // The crashed process: claim taken, summary queued, never completed.
        let pool = check::open(ws).await.unwrap();
        assert_eq!(check::claim_occurrence(&pool, key, now, chrono::Duration::minutes(30)).await.unwrap(), Claim::Acquired);
        let wa = nucleus_core::whatsapp_queue::open(ws).await.unwrap();
        reminders::store::enqueue_whatsapp_once(&wa, "x", "line", &format!("vault-check:{key}")).await.unwrap();

        let c = ctx(ws);
        // Next hourly wake, claim still fresh: another process may be running.
        let o = run_scheduled(&c, now + chrono::Duration::minutes(10), fake_report).await.unwrap();
        assert!(matches!(o, ScheduledOutcome::Busy), "{o:?}");
        // Later wake: the stale claim is taken over; no second summary.
        let o = run_scheduled(&c, now + chrono::Duration::hours(1), fake_report).await.unwrap();
        assert!(matches!(o, ScheduledOutcome::Ran(_)), "{o:?}");
        assert_eq!(queued(ws).await.len(), 1);
    }
}
