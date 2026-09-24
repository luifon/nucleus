//! `vault-check` — the weekly vault check (ADR-035).
//!
//! A deterministic pass over the Obsidian vault: no Claude session. The
//! analysis, the safe fixes and the run history live in
//! `nucleus_core::vault::check`; this crate is the command, the weekly
//! schedule gate, and the WhatsApp summary.
//!
//! Examples:
//!   nucleus vault-check                  # report only, recorded in history
//!   nucleus vault-check --apply          # also apply the safe fixes
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

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use clap::Parser;
use nucleus_core::config::Settings;
use nucleus_core::vault::check::{self, CheckOptions, CheckReport};
use nucleus_core::vault::exclude::Exclusions;
use std::path::Path;

const WATERMARK_KEY: &str = "vault-check.scheduled";
const WHATSAPP_DB_PATH: &str = "memory/whatsapp.db";
/// Dashboard route that shows the latest report.
const DASHBOARD_ROUTE: &str = "/vault/check";

#[derive(Parser)]
#[command(
    name = "vault-check",
    about = "Structural check of the Obsidian vault (ADR-035): duplicates, broken links, orphans, stale inbox, frontmatter, sources, empty files"
)]
struct Cli {
    /// Apply the safe fixes (delete empty untitled files nothing links to;
    /// add missing `created` from the file's birth time). Default: report only.
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

    let (apply, notify, trigger) = if cli.scheduled {
        if !scheduled_due(&workspace_root, &cfg.cron, Utc::now()).await? {
            return Ok(());
        }
        (cfg.scheduled_apply, cfg.notify, "scheduled")
    } else {
        (cli.apply, cli.notify, "manual")
    };

    let vault = settings.obsidian.vault_dir();
    let ex = Exclusions::from_config(&settings.vault_search)?;
    let opts = CheckOptions::from_config(cfg, apply, trigger)?;
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

    if notify {
        let link = settings
            .public_urls
            .nucleus
            .as_deref()
            .map(|u| format!("{}{DASHBOARD_ROUTE}", u.trim_end_matches('/')));
        let line = check::summary_line(&report.counts, link.as_deref());
        enqueue_summary(&workspace_root, &line).await?;
    }

    if cli.scheduled {
        nucleus_core::chore_state::set_watermark(&workspace_root, WATERMARK_KEY, &Utc::now().to_rfc3339())
            .await?;
        let _ = nucleus_core::diary::record_observation(
            &workspace_root,
            "vault-check",
            "weekly",
            &check::summary_line(&report.counts, None),
            nucleus_core::diary::Tag::Routine,
        );
    }
    Ok(())
}

/// Weekly gate. The first scheduled wake after install only records the
/// watermark, so installing the job does not send a report immediately.
async fn scheduled_due(workspace_root: &Path, cron: &str, now: DateTime<Utc>) -> Result<bool> {
    let wm = nucleus_core::chore_state::watermark(workspace_root, WATERMARK_KEY).await?;
    let Some(last) = wm.and_then(|s| DateTime::parse_from_rfc3339(&s).ok()) else {
        nucleus_core::chore_state::set_watermark(workspace_root, WATERMARK_KEY, &now.to_rfc3339()).await?;
        tracing::info!("vault-check: schedule armed; first run at the next `{cron}` match");
        return Ok(false);
    };
    is_due(cron, last.with_timezone(&Utc), now)
}

/// Due when the cron has a match after `last` and at or before `now`.
fn is_due(cron: &str, last: DateTime<Utc>, now: DateTime<Utc>) -> Result<bool> {
    let next = reminders::store::next_match_utc(cron, last, reminders::store::nucleus_tz())
        .with_context(|| format!("[vault_check] cron {cron:?}"))?;
    Ok(next <= now)
}

/// Queue the summary for the operator's DM (ADR-005b), through the same
/// outbound queue the reminders use (ADR-020 queue-table pattern).
async fn enqueue_summary(workspace_root: &Path, line: &str) -> Result<()> {
    let Some(target) = reminders::operator_whatsapp_dm() else {
        tracing::warn!("vault-check: WHATSAPP_ALLOWED_DM_JIDS is empty; summary not sent");
        return Ok(());
    };
    let pool = reminders::store::open_whatsapp_db(&workspace_root.join(WHATSAPP_DB_PATH)).await?;
    let id = reminders::store::enqueue_whatsapp(&pool, &target, line, "vault-check").await?;
    tracing::info!(queue_id = id, "vault-check: summary enqueued");
    Ok(())
}

fn print_report(r: &CheckReport, show: usize) {
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
        "  duplicates {} · broken links {} · orphans {} · stale inbox {} · frontmatter {} · unknown source {} · empty files {} · fixed {}",
        c.duplicates, c.broken_links, c.orphans, c.stale_inbox, c.missing_frontmatter, c.unknown_source, c.empty_files, c.fixed
    );
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
}
