//! `nucleus events` — the generic event intake for scripts (ADR-036).
//!
//! A script that watches something (a homelab alert, a mailbox) reports it
//! as an event in the common record; the store deduplicates by
//! `(source, external id)`, so a script can report the same thing on every
//! run. An event becomes a pipeline item only with `--accept` (the
//! operator's own terminal) and a `--project` that is a configured repo.
//!
//!   nucleus events emit --source homelab --id disk-sda-2026-09-24 \
//!     --title "Disk sda above 90%" --body - <<'EOF'
//!   …details…
//!   EOF
//!   nucleus events list

use crate::caller::{self, Role};
use crate::intake::event::NewEvent;
use crate::intake::pipeline::{self, Ctx};
use crate::intake::store;
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::io::Read;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "events", about = "Report and list intake events (ADR-036)")]
struct Cli {
    #[arg(long, global = true)]
    workspace_root: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Record one event (a second report with the same source and id
    /// updates it).
    Emit {
        /// Source name (letters, digits, - and _), e.g. homelab, mail.
        #[arg(long)]
        source: String,
        /// The event's id within its source.
        #[arg(long = "id")]
        external_id: String,
        #[arg(long)]
        title: String,
        /// Details; `-` reads stdin.
        #[arg(long, default_value = "")]
        body: String,
        /// The repo (`owner/name`) or project the event is about.
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value = "event")]
        kind: String,
        #[arg(long)]
        author: Option<String>,
        #[arg(long = "label")]
        labels: Vec<String>,
        #[arg(long)]
        url: Option<String>,
        /// open | closed.
        #[arg(long, default_value = "open")]
        state: String,
        /// Start pipeline work on it (operator only; needs a configured
        /// repo as --project).
        #[arg(long)]
        accept: bool,
    },
    /// Recent events, newest first.
    List {
        #[arg(long, default_value_t = 30)]
        limit: i64,
        #[arg(long)]
        json: bool,
    },
}

pub async fn run(args: Vec<std::ffi::OsString>) -> Result<()> {
    let cli = Cli::parse_from(args);
    let settings = crate::config::Settings::load().context("loading settings")?;
    let ws = match cli.workspace_root {
        Some(p) => p,
        None => settings.workspace_root()?,
    };
    let caller = caller::detect(&ws).await?;
    match (&caller.role, &cli.cmd) {
        (Role::Operator, _) => {}
        (Role::Detached, Cmd::Emit { accept: false, .. }) | (Role::Detached, Cmd::List { .. }) => {}
        (Role::Detached, Cmd::Emit { accept: true, .. }) => {
            bail!("--accept starts pipeline work; only the operator's terminal may use it")
        }
        (role, _) => bail!("this caller ({role:?}) cannot report events"),
    }
    if caller.inbound_hop > 0 {
        bail!("this turn is reacting to a message from another agent; events need the operator");
    }
    let ctx = Ctx::open(&ws, &settings).await?;
    match cli.cmd {
        Cmd::Emit { source, external_id, title, body, project, kind, author, labels, url, state, accept } => {
            let body = if body == "-" {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                s
            } else {
                body
            };
            let raw = serde_json::json!({ "emitted_by": "nucleus events emit" });
            let e = NewEvent {
                source,
                external_id,
                project,
                kind,
                title,
                body,
                author,
                labels,
                url,
                state,
                created_at: Some(crate::timestamp::now()),
                updated_at: Some(crate::timestamp::now()),
                raw,
                accepted: accept,
            };
            let (ev, item) = pipeline::record_event(&ctx, &e).await?;
            println!("event {} recorded ({} {})", ev.id, ev.source, ev.external_id);
            match item {
                Some(i) => println!("pipeline item #{} created on {}", i.id, i.repo),
                None if accept && ev.project.as_deref().and_then(|p| ctx.cfg.repo(p)).is_none() => {
                    println!("not a pipeline item: --project is not a configured repo")
                }
                None => {}
            }
        }
        Cmd::List { limit, json } => {
            let events = store::list_events(&ctx.db, limit).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&events)?);
            } else {
                for e in events {
                    println!(
                        "{:<5} {:<10} {:<28} {:<6} {}{}",
                        e.id,
                        e.source,
                        e.external_id,
                        e.state,
                        e.title,
                        if e.accepted { " [accepted]" } else { "" }
                    );
                }
            }
        }
    }
    Ok(())
}
