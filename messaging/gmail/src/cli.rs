//! CLI surface for the gmail-metabolism binary.
//!
//! Default subcommand is `metabolize` (the daily JARVIS sweep); the
//! launchd plist invokes the binary with no args and that's what runs.
//! `killlist` lets the operator manually seed senders via the shell.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use nucleus_core::config::Settings;
use std::path::Path;

use crate::{metabolize, store};

const DB_PATH: &str = "memory/gmail.db";

#[derive(Parser)]
#[command(name = "gmail-metabolism", about = "JARVIS-driven Gmail inbox metabolism")]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// (default) Run the daily JARVIS sweep.
    Metabolize,
    /// Manage the auto-trash kill-list.
    Killlist {
        #[command(subcommand)]
        action: KilllistAction,
    },
}

#[derive(Subcommand)]
enum KilllistAction {
    /// Add a sender so future mail from them is auto-trashed.
    Add {
        email: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Print every sender currently on the list.
    List,
}

pub async fn run() -> Result<()> {
    let settings = Settings::load().context("loading settings")?;
    let workspace_root = std::env::current_dir()?;

    let cli = Cli::parse();
    match cli.command.unwrap_or(Cmd::Metabolize) {
        Cmd::Metabolize => metabolize::run(&settings, &workspace_root).await,
        Cmd::Killlist { action } => match action {
            KilllistAction::Add { email, reason } => {
                killlist_add(&workspace_root, &email, reason.as_deref()).await
            }
            KilllistAction::List => killlist_list(&workspace_root).await,
        },
    }
}

async fn killlist_add(workspace_root: &Path, email: &str, reason: Option<&str>) -> Result<()> {
    let email = email.trim();
    if email.is_empty() || !email.contains('@') {
        bail!("invalid sender {:?}", email);
    }
    let pool = store::open(&workspace_root.join(DB_PATH)).await?;
    let added = store::killlist_add(&pool, email, reason, "manual").await?;
    if added {
        println!("added {email}");
    } else {
        println!("{email} already on kill-list");
    }
    Ok(())
}

async fn killlist_list(workspace_root: &Path) -> Result<()> {
    let pool = store::open(&workspace_root.join(DB_PATH)).await?;
    let rows = store::killlist(&pool).await?;
    if rows.is_empty() {
        println!("(empty)");
        return Ok(());
    }
    for email in rows {
        println!("{email}");
    }
    Ok(())
}
