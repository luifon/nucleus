//! `nucleus intake` — the issue pipeline (ADR-036).
//!
//! `tick` is the driver: launchd runs it every minute
//! (`tools/launchd/intake-tick.plist.example`), the WhatsApp bot and the
//! dashboard run it after an operator reply. The other commands inspect
//! items and take the operator's decisions.
//!
//! Who may do what (`crate::caller`):
//! - the operator: every command;
//! - the WhatsApp DM chat session: `list`, `show`, and `cancel` (not in a
//!   turn that read an agent message). Approvals are the operator's own
//!   messages in the item's thread, read by code, never a session's
//!   command;
//! - a detached process (launchd, the bot, the dashboard): `tick`;
//! - workers and every other Nucleus session: nothing.

use crate::caller::{self, Caller, Role};
use crate::intake::pipeline::{self, Ctx, Refusal};
use crate::intake::stage::EvalResult;
use crate::intake::store;
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::io::Read;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "intake", about = "Issue pipeline: items, replies, approvals (ADR-036)")]
struct Cli {
    /// Workspace root (defaults to NUCLEUS_WORKSPACE_ROOT from settings).
    #[arg(long, global = true)]
    workspace_root: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Poll the sources whose interval passed, read operator replies,
    /// advance every open item.
    Tick {
        /// Poll every source now, whatever its interval.
        #[arg(long)]
        poll: bool,
    },
    /// Items, newest first (open ones unless --all).
    List {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// One item: stage, eval, plan, thread, tasks.
    Show {
        item: String,
        #[arg(long)]
        json: bool,
    },
    /// Write in an item's thread (refinement only). `--text -` reads stdin.
    Reply {
        item: String,
        #[arg(long)]
        text: String,
    },
    /// Approve the item's latest plan; implementation starts.
    ApprovePlan {
        item: String,
        /// The plan version you read; refused when it is not the latest.
        #[arg(long)]
        version: Option<u32>,
    },
    /// Approve the proposed issue comment (optionally replacing its text).
    ApproveComment {
        item: String,
        #[arg(long)]
        text: Option<String>,
    },
    /// Close the review without commenting on the issue.
    SkipComment { item: String },
    /// Stop an item and its running task.
    Cancel { item: String },
    /// Resume a failed item at the stage it failed in.
    Retry { item: String },
}

fn authorize(caller: &Caller, cmd: &Cmd) -> Result<()> {
    let mutating = !matches!(cmd, Cmd::List { .. } | Cmd::Show { .. } | Cmd::Tick { .. });
    match &caller.role {
        Role::Operator => {}
        Role::Chat { origin, .. } if origin == "whatsapp-dm" => match cmd {
            Cmd::List { .. } | Cmd::Show { .. } | Cmd::Cancel { .. } => {}
            _ => bail!(
                "a chat session can list, show and cancel items; approvals and replies are the \
                 operator's own messages in the item's thread (`#<n> approve`), or the dashboard"
            ),
        },
        Role::Detached => {
            if !matches!(cmd, Cmd::Tick { .. }) {
                bail!("a process with no terminal and no session may only run `intake tick`");
            }
        }
        Role::Worker { .. } => bail!("background task workers cannot use the intake CLI"),
        Role::Chat { .. } | Role::UnscopedChat => bail!("this chat session cannot use the intake CLI"),
        Role::Session { kind } => bail!("this Nucleus session ({kind}) cannot use the intake CLI"),
        Role::Unknown(reason) => bail!("the intake CLI cannot identify its caller: {reason}"),
    }
    if mutating && caller.inbound_hop > 0 {
        bail!(
            "this turn is reacting to a message from another agent (hop:{}); changing an item needs \
             a request from the operator",
            caller.inbound_hop
        );
    }
    Ok(())
}

fn item_number(s: &str) -> Result<i64> {
    s.trim().trim_start_matches('#').parse().with_context(|| format!("{s:?} is not an item number (#12 or 12)"))
}

/// Entry point for `nucleus intake`. `args` includes argv[0].
pub async fn run(args: Vec<std::ffi::OsString>) -> Result<()> {
    let cli = Cli::parse_from(args);
    let settings = crate::config::Settings::load().context("loading settings")?;
    let ws = match cli.workspace_root {
        Some(p) => p,
        None => settings.workspace_root()?,
    };
    let caller = caller::detect(&ws).await?;
    authorize(&caller, &cli.cmd)?;
    let ctx = Ctx::open(&ws, &settings).await?;
    let via = match caller.role {
        Role::Chat { .. } => "whatsapp-session",
        _ => "cli",
    };
    let result = match cli.cmd {
        Cmd::Tick { poll } => {
            if !ctx.cfg.enabled {
                println!("intake is disabled ([intake] enabled = false)");
                return Ok(());
            }
            let r = pipeline::tick(&ctx, poll).await?;
            for p in &r.polled {
                println!("polled {p}");
            }
            for n in &r.new_items {
                println!("new item #{n}");
            }
            for s in &r.steps {
                println!("{s}");
            }
            for e in &r.errors {
                eprintln!("error: {e}");
            }
            Ok(())
        }
        Cmd::List { all, json } => {
            let items = store::list_items(&ctx.db, !all, 200).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&items)?);
            } else if items.is_empty() {
                println!("no items");
            } else {
                for i in &items {
                    println!(
                        "#{:<4} {:<14} {} — {}{}",
                        i.id,
                        i.stage,
                        i.repo,
                        i.title,
                        i.pr_url.as_deref().map(|u| format!(" · {u}")).unwrap_or_default()
                    );
                }
            }
            Ok(())
        }
        Cmd::Show { item, json } => {
            let n = item_number(&item)?;
            let it = store::item(&ctx.db, n).await?;
            let ev = store::event(&ctx.db, it.event_id).await?;
            let msgs = store::messages(&ctx.db, n).await?;
            let tasks = store::item_tasks(&ctx.db, n).await?;
            let log = store::transitions(&ctx.db, n).await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "item": it, "event": ev, "messages": msgs, "tasks": tasks, "transitions": log
                    }))?
                );
                return Ok(());
            }
            println!("#{} {} — {} ({})", it.id, it.stage, it.title, ev.external_id);
            if let Some(u) = &ev.url {
                println!("source: {u}");
            }
            if let Some(r) = &it.stale_reason {
                println!("STALE: {r}");
                println!("  nothing more is done for this item; remove and add the `{}` label again for a new item", ctx.cfg.label);
            } else if let Some(e) = &it.error {
                println!("error: {e}");
            }
            if let (Some(g), Some(a)) = (&it.gate_event_id, &it.gate_actor) {
                println!("gate: {g} by {a} at {}", it.gate_at.as_deref().unwrap_or("?"));
            }
            if let Some(e) = it.eval_json.as_deref().and_then(|j| serde_json::from_str::<EvalResult>(j).ok()) {
                println!("eval: {} (agent: {}) — {}", e.effective, e.classification, e.summary);
                for r in e.reasons.iter().chain(e.escalations.iter()) {
                    println!("  - {r}");
                }
            }
            if let Some(p) = &it.approved_plan {
                println!("approved plan v{}:\n{p}", it.approved_version.unwrap_or(0));
            } else if let Some(p) = &it.plan_draft {
                println!("proposed plan v{} (not approved):\n{p}", it.plan_version);
            }
            if let Some(b) = &it.branch {
                println!("branch: {b}");
            }
            if let Some(t) = &it.tests_status {
                println!("tests: {t}");
            }
            if let Some(u) = &it.pr_url {
                println!("draft PR: {u}");
            }
            if it.comment_state != "none" {
                println!("issue comment: {}", it.comment_state);
            }
            println!("WhatsApp thread: {}", it.surface);
            println!("\nthread (last 15):");
            for m in msgs.iter().rev().take(15).collect::<Vec<_>>().into_iter().rev() {
                println!("  [{} {} via {}] {}", m.at, m.author, m.via, crate::intake::clip(&m.body, 300).replace('\n', " "));
            }
            println!("\ntasks:");
            for t in &tasks {
                println!("  {} {}", &t.task_id[..8.min(t.task_id.len())], t.stage);
            }
            println!("\nstages:");
            for t in &log {
                println!("  {} {} → {} ({})", t.at, t.from_stage.as_deref().unwrap_or("-"), t.to_stage, t.reason);
            }
            Ok(())
        }
        Cmd::Reply { item, text } => {
            let text = if text == "-" {
                let mut s = String::new();
                std::io::stdin().read_to_string(&mut s)?;
                s
            } else {
                text
            };
            let n = item_number(&item)?;
            pipeline::reply(&ctx, n, &text, via).await.map(|_| println!("message added to item #{n}"))
        }
        Cmd::ApprovePlan { item, version } => {
            let n = item_number(&item)?;
            pipeline::approve_plan(&ctx, n, version, via)
                .await
                .map(|i| println!("plan v{} of item #{n} approved", i.approved_version.unwrap_or(0)))
        }
        Cmd::ApproveComment { item, text } => {
            let n = item_number(&item)?;
            pipeline::approve_comment(&ctx, n, text, via)
                .await
                .map(|_| println!("comment of item #{n} approved; the next tick posts it"))
        }
        Cmd::SkipComment { item } => {
            let n = item_number(&item)?;
            pipeline::skip_comment(&ctx, n, via).await.map(|_| println!("item #{n} closes without a comment"))
        }
        Cmd::Cancel { item } => {
            let n = item_number(&item)?;
            pipeline::cancel(&ctx, n, via).await.map(|_| println!("item #{n} cancelled"))
        }
        Cmd::Retry { item } => {
            let n = item_number(&item)?;
            pipeline::retry(&ctx, n, via).await.map(|i| println!("item #{n} resumed at {}", i.stage))
        }
    };
    match result {
        Err(e) if e.downcast_ref::<Refusal>().is_some() => bail!("{e}"),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(role: Role, hop: u8) -> Caller {
        Caller { role, agent: None, session_id: None, inbound_hop: hop }
    }

    #[test]
    fn authorization_by_caller() {
        let chat = Role::Chat { origin: "whatsapp-dm".into(), chat: "5511999999999@s.whatsapp.net".into() };
        let show = || Cmd::Show { item: "1".into(), json: false };
        let approve = || Cmd::ApprovePlan { item: "1".into(), version: None };
        let cancel = || Cmd::Cancel { item: "1".into() };
        let tick = || Cmd::Tick { poll: false };
        assert!(authorize(&caller(Role::Operator, 0), &approve()).is_ok());
        assert!(authorize(&caller(Role::Operator, 1), &approve()).is_err(), "reacting to an agent message");
        assert!(authorize(&caller(chat.clone(), 0), &show()).is_ok());
        assert!(authorize(&caller(chat.clone(), 0), &cancel()).is_ok());
        assert!(authorize(&caller(chat.clone(), 1), &cancel()).is_err());
        assert!(authorize(&caller(chat.clone(), 0), &approve()).is_err(), "a session never approves");
        assert!(authorize(&caller(chat, 0), &Cmd::Reply { item: "1".into(), text: "x".into() }).is_err());
        assert!(authorize(&caller(Role::Detached, 0), &tick()).is_ok());
        assert!(authorize(&caller(Role::Detached, 0), &show()).is_err());
        for role in [
            Role::Worker { task_id: None },
            Role::UnscopedChat,
            Role::Session { kind: "agent".into() },
            Role::Unknown("x".into()),
        ] {
            assert!(authorize(&caller(role.clone(), 0), &show()).is_err(), "{role:?}");
            assert!(authorize(&caller(role, 0), &tick()).is_err());
        }
        assert_eq!(item_number("#12").unwrap(), 12);
        assert!(item_number("x").is_err());
    }
}
