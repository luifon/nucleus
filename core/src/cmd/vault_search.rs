//! `vault-search` — full-text search over the Obsidian vault (ADR-035).
//! Callable from any session via Bash, like `session-search`.
//!
//! Examples:
//!   nucleus vault-search "rocket engine budget"
//!   nucleus vault-search "weekly review" --bucket 3-Projects --limit 5
//!   nucleus vault-search "standup" --json
//!   nucleus vault-search --reindex          # refresh the index only
//!
//! Plain words must all match (punctuation is ignored). When no note has
//! every word, the results fall back to notes with any of them and say so.
//! FTS5 syntax (`OR`, `NOT`, `"exact phrase"`, `prefix*`) is honored.
//! Porter stemming and accent folding are on (`orcamento` finds `orçamento`).
//! Credential notes and excluded folders are never indexed or returned.

use crate::vault::{exclude::Exclusions, index};
use anyhow::{Context, Result, bail};
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "vault-search",
    about = "Search the Obsidian vault by content (ADR-035); the index updates incrementally on every run"
)]
struct Cli {
    /// Search words. Optional only with --reindex.
    query: Vec<String>,
    /// Restrict to a bucket or folder path prefix (`3-Projects`,
    /// `4-Areas/Health`).
    #[arg(long)]
    bucket: Option<String>,
    /// Maximum results.
    #[arg(long, default_value_t = 10)]
    limit: i64,
    /// Print JSON (`{mode, hits:[{path,title,display,bucket,created,source,snippet,score}]}`).
    #[arg(long)]
    json: bool,
    /// Refresh the index, print its counts, and exit.
    #[arg(long)]
    reindex: bool,
}

pub async fn run(args: Vec<std::ffi::OsString>) -> Result<()> {
    crate::init_tracing();
    let cli = Cli::parse_from(args);
    let settings = crate::config::Settings::load().context("loading settings")?;
    let workspace_root = settings.workspace_root()?;
    let vault = settings.obsidian.vault_dir();
    let ex = Exclusions::from_config(&settings.vault_search)?;
    let pool = index::open(&workspace_root).await?;

    let t0 = std::time::Instant::now();
    let stats = index::update(&pool, &vault, &ex).await?;
    if cli.reindex || stats.indexed + stats.removed > 0 {
        eprintln!(
            "index: {} notes scanned, {} (re)indexed, {} removed, {} unchanged, {} excluded by path, {} excluded as credentials ({} ms)",
            stats.scanned,
            stats.indexed,
            stats.removed,
            stats.unchanged,
            stats.path_excluded,
            stats.content_excluded,
            t0.elapsed().as_millis()
        );
    }
    if cli.reindex && cli.query.is_empty() {
        return Ok(());
    }
    let query = cli.query.join(" ");
    if query.trim().is_empty() {
        bail!("provide search words, or --reindex");
    }

    let opts = index::SearchOpts { bucket: cli.bucket.as_deref(), limit: cli.limit };
    let result = index::search(&pool, &query, &opts, &ex).await?;
    if cli.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    if result.hits.is_empty() {
        println!("no notes match {query:?}");
        return Ok(());
    }
    if result.mode == "any" {
        println!("(no note contains every word; showing notes that contain some of them)\n");
    }
    for (i, h) in result.hits.iter().enumerate() {
        let created = h.created.as_deref().map(|c| format!("  created {c}")).unwrap_or_default();
        println!("{:>2}. {} — {}{}", i + 1, h.display, h.title, created);
        println!("    {}", h.path);
        println!("    {}", h.snippet);
    }
    Ok(())
}
