//! `session-search` — FTS5 search over this workspace's session
//! transcripts (ADR-023). Callable from any session via Bash; skills and
//! personas can name it directly, same pattern as the `reminders` CLI.
//!
//! Examples:
//!   session-search "consórcio adm fee"
//!   session-search "storage state" --agent whatsapp --days 30
//!   session-search --reindex            # index refresh only, no query
//!   session-search --prune              # report junk candidates (dry-run)
//!   session-search --prune --apply      # actually delete old junk
//!
//! Query syntax is FTS5: bare words AND together, `OR` works, quoted
//! phrases match exactly. Porter stemming is on (deciding ~ decide).

use anyhow::{Result, bail};
use clap::Parser;
use nucleus_core::session_index;

#[derive(Parser)]
#[command(
    name = "session-search",
    about = "Search past session transcripts (ADR-023); index updates incrementally on every run"
)]
struct Cli {
    /// FTS5 query. Optional only with --reindex / --prune.
    query: Option<String>,
    /// Filter by run-log agent label (prefix match: "whatsapp" covers
    /// whatsapp-dm etc.).
    #[arg(long)]
    agent: Option<String>,
    /// Only sessions whose transcript changed in the last N days.
    #[arg(long)]
    days: Option<i64>,
    /// Max hits.
    #[arg(long, default_value_t = 12)]
    limit: i64,
    /// Refresh the index and exit (no query needed).
    #[arg(long)]
    reindex: bool,
    /// Report ineligible transcripts old enough to delete (dry-run
    /// unless --apply).
    #[arg(long)]
    prune: bool,
    /// Actually delete what --prune reports.
    #[arg(long, requires = "prune")]
    apply: bool,
    /// Age threshold for --prune, in days.
    #[arg(long, default_value_t = 14)]
    max_age_days: i64,
}

#[tokio::main]
async fn main() -> Result<()> {
    nucleus_core::init_tracing();
    let cli = Cli::parse();
    let workspace_root = std::env::current_dir()?;
    let pool = session_index::open(&workspace_root).await?;

    let stats = session_index::update_index(&pool, &workspace_root).await?;
    if stats.indexed + stats.ineligible > 0 || cli.reindex {
        eprintln!(
            "index: {} scanned, {} (re)indexed, {} ineligible, {} unchanged",
            stats.scanned, stats.indexed, stats.ineligible, stats.skipped_unchanged
        );
    }

    if cli.prune {
        let p = session_index::prune_junk(&pool, &workspace_root, cli.apply, cli.max_age_days)
            .await?;
        let tail = if p.dry_run {
            " — pass --apply to delete".to_string()
        } else {
            format!(", {} deleted", p.deleted)
        };
        println!(
            "prune{}: {} junk transcript(s) older than {}d{}",
            if p.dry_run { " (dry-run)" } else { "" },
            p.candidates,
            cli.max_age_days,
            tail
        );
        return Ok(());
    }
    if cli.reindex {
        return Ok(());
    }

    let Some(query) = cli.query.as_deref() else {
        bail!("provide a query, or --reindex / --prune");
    };
    let hits = session_index::search(&pool, query, cli.agent.as_deref(), cli.days, cli.limit)
        .await?;
    if hits.is_empty() {
        println!("no hits for {:?}", query);
        return Ok(());
    }
    for h in &hits {
        let date = h.session_ts.get(..10).unwrap_or("?");
        let agent = if h.agent.is_empty() { "?" } else { &h.agent };
        println!("{date}  {agent:<14} {role:<9} {snippet}", role = h.role, snippet = h.snippet.replace('\n', " "));
        println!("           └ session {}", h.session_id);
    }
    Ok(())
}
