//! `nucleus usage` — usage accounting (ADR-034).
//!
//!   nucleus usage refresh           # ingest new transcript bytes (incremental)
//!   nucleus usage refresh --full    # re-read every transcript from the start
//!   nucleus usage report --days 30  # totals, top projects, models, agents
//!
//! `refresh` is the only writer of `memory/usage.db`; the dashboard spawns
//! it and the distiller runs it daily. A second concurrent refresh exits
//! with an error instead of writing.

use crate::usage;
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "usage", about = "Token and estimated-cost accounting for Claude Code and Codex sessions (ADR-034)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Ingest new transcript data into memory/usage.db.
    Refresh {
        /// Re-read every file from the start (idempotent; slow).
        #[arg(long)]
        full: bool,
    },
    /// Print totals for a range.
    Report {
        /// Days back from today, 0 = all time.
        #[arg(long, default_value_t = 30)]
        days: u32,
        /// Rows per table.
        #[arg(long, default_value_t = 10)]
        top: usize,
        /// Tool filter: all, claude or codex.
        #[arg(long, default_value = "all")]
        vendor: String,
    },
}

fn tok(n: i64) -> String {
    let f = n as f64;
    if f >= 1e9 {
        format!("{:.2}B", f / 1e9)
    } else if f >= 1e6 {
        format!("{:.1}M", f / 1e6)
    } else if f >= 1e3 {
        format!("{:.1}k", f / 1e3)
    } else {
        n.to_string()
    }
}

pub async fn run(args: Vec<std::ffi::OsString>) -> Result<()> {
    crate::init_tracing();
    let cli = Cli::parse_from(args);
    let settings = crate::config::Settings::load().context("loading settings")?;
    let root = settings.workspace_root()?;
    match cli.cmd {
        Cmd::Refresh { full } => {
            let s = usage::refresh(&root, &settings.usage, usage::RefreshOptions { full }).await?;
            println!(
                "usage refresh: {} files seen, {} read ({:.1} MB), {} records, {} sessions labeled, \
                 {} cost-state runs (${:.2} Claude Code estimate, table drift ${:+.2}), {:.1}s",
                s.files_seen,
                s.files_read,
                s.bytes_read as f64 / 1e6,
                s.records,
                s.sessions_labeled,
                s.reconcile.runs,
                s.reconcile.cost_state_usd,
                s.reconcile.adjustment_usd,
                s.elapsed.as_secs_f64()
            );
        }
        Cmd::Report { days, top, vendor } => {
            let v = match vendor.as_str() {
                "all" => usage::query::VendorFilter::All,
                "claude" => usage::query::VendorFilter::Claude,
                "codex" => usage::query::VendorFilter::Codex,
                other => anyhow::bail!("--vendor must be all, claude or codex (got {other})"),
            };
            let Some(pool) = usage::open_read_only(&root).await? else {
                println!("no usage data yet — run `nucleus usage refresh`");
                return Ok(());
            };
            let sum = usage::query::summary(&pool, days, v).await?;
            println!("range: {} (estimates at API list price)", sum.range.label);
            for v in &sum.by_vendor {
                println!(
                    "  {:<7} {:>9} tokens  ${:>10.2}  {} sessions  (cache read {}, unpriced {})",
                    v.vendor,
                    tok(v.totals.tokens),
                    v.totals.cost_usd,
                    v.sessions,
                    tok(v.totals.cache_read),
                    tok(v.totals.unpriced_tokens)
                );
            }
            println!("projects:");
            for p in usage::query::projects(&pool, days, v).await?.iter().take(top) {
                println!(
                    "  {:<28} {:>9}  ${:>10.2}  (claude ${:.2} / codex ${:.2})  {} sessions",
                    p.project,
                    tok(p.totals.tokens),
                    p.totals.cost_usd,
                    p.claude_cost_usd,
                    p.codex_cost_usd,
                    p.sessions
                );
            }
            println!("models:");
            for m in sum.models.iter().take(top) {
                println!("  {:<6} {:<28} {:>9}  ${:>10.2}", m.vendor, m.model, tok(m.totals.tokens), m.totals.cost_usd);
            }
            let ws = root.to_string_lossy();
            let n = usage::query::nucleus(&pool, days, &ws, v).await?;
            println!("nucleus agents:");
            for a in n.agents.iter().take(top) {
                println!("  {:<20} ${:>9.2}  {} sessions  (last 30d ${:.2})", a.agent, a.totals.cost_usd, a.sessions, a.cost_30d);
            }
            println!("reminders (last 30 days):");
            for r in n.reminders.iter().take(top) {
                println!(
                    "  #{:<4} {:<32} {} fires  ${:.2}  cron {}",
                    r.reminder_id,
                    r.title.as_deref().unwrap_or("?"),
                    r.sessions_30d,
                    r.cost_30d,
                    r.cron.as_deref().unwrap_or("-")
                );
            }
            let l = usage::query::limits(&pool, days, v).await?;
            println!("limit/error events: {}", l.events.len());
        }
    }
    Ok(())
}
