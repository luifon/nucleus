//! `nucleus tasks` — the background-task CLI (ADR-033). The chat sessions
//! call it to start work that should not hold the conversation, and to
//! answer the operator's questions about that work; the operator can use it
//! directly too.
//!
//! What a caller may do depends on who it is (`crate::caller`): the operator
//! sees every task; a WhatsApp chat session starts tasks for its own chat
//! (the origin comes from its scope token, not from arguments) and sees only
//! those; a background worker may not use this CLI; a session whose current
//! turn is reacting to an agent message may not start or cancel tasks.
//!
//! Examples:
//!   nucleus tasks start --title "Rebuild the report" --requested-by operator \
//!     --brief - <<'EOF'
//!   …full brief…
//!   EOF
//!   nucleus tasks list
//!   nucleus tasks status 3f2a
//!   nucleus tasks output 3f2a
//!   nucleus tasks cancel 3f2a

use crate::caller::{self, Caller, Role};
use crate::tasks::{self, NewTask, Scope, Task};
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::io::Read;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "tasks", about = "Background tasks: start, inspect, cancel (ADR-033)")]
struct Cli {
    /// Workspace root (defaults to NUCLEUS_WORKSPACE_ROOT from settings).
    #[arg(long, global = true)]
    workspace_root: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a task and start its worker. Prints the task id.
    Start {
        /// Short name shown in lists and in the result message.
        #[arg(long)]
        title: String,
        /// The full instructions for the worker. `-` reads them from stdin.
        #[arg(long, conflicts_with = "brief_file")]
        brief: Option<String>,
        /// Read the brief from this file.
        #[arg(long)]
        brief_file: Option<PathBuf>,
        /// Where the task came from and where its result goes:
        /// whatsapp-dm | discord-home | cli | dashboard | pipeline. A chat
        /// session's origin is set by its scope and cannot be given here.
        #[arg(long)]
        origin: Option<String>,
        /// Venue-specific pointer back to the request; for whatsapp-dm, the
        /// chat the result goes to (default: the operator's DM chat).
        #[arg(long)]
        origin_ref: Option<String>,
        /// model | operator | pipeline | cli.
        #[arg(long, default_value = "cli")]
        requested_by: String,
        /// Free-form task kind (general, research, report, implementation…).
        #[arg(long, default_value = "general")]
        kind: String,
        /// Parent task id (full or prefix).
        #[arg(long)]
        parent: Option<String>,
        /// Reference to attach, as `rel=target` (repeatable), e.g. `issue=org/repo#12`.
        #[arg(long = "link")]
        links: Vec<String>,
    },
    /// List tasks, newest first.
    List {
        /// Include finished tasks (default: queued and running only, plus the
        /// last 10 finished).
        #[arg(long)]
        all: bool,
        #[arg(long, default_value_t = 30)]
        limit: i64,
        #[arg(long)]
        json: bool,
    },
    /// Show one task: state, timings, progress log.
    Status {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Print the result (or, while running, the latest progress).
    Output { id: String },
    /// Stop a task. Queued tasks stop at once; running tasks within seconds.
    Cancel { id: String },
    /// Mark tasks whose worker is gone as interrupted and report them.
    Sweep,
    /// Internal: the detached worker process for one task. Needs the
    /// one-time run token `tasks start` writes to its stdin.
    #[command(hide = true)]
    Run { id: String },
}

/// What the caller may do, from `crate::caller` (pure; tested below).
fn authorize(caller: &Caller, cmd: &Cmd) -> Result<Scope> {
    if matches!(cmd, Cmd::Run { .. }) {
        bail!("`tasks run` is internal: the worker `tasks start` launches runs it");
    }
    let scope = match &caller.role {
        Role::Worker { .. } => bail!(
            "background task workers cannot use the tasks CLI: do the work in this session; \
             the result is delivered automatically (ADR-033)"
        ),
        Role::UnscopedChat => bail!(
            "this chat session has no valid task scope (only the WhatsApp DM chat sessions may \
             use background tasks, and a scope ends with its session)"
        ),
        Role::Session { kind } => bail!(
            "this Nucleus session ({kind}) cannot use background tasks: only the operator and \
             the WhatsApp DM chat sessions can (ADR-033)"
        ),
        Role::Detached => {
            if !matches!(cmd, Cmd::Sweep) {
                bail!(
                    "the tasks CLI refuses a process with no terminal and no session (only \
                     `tasks sweep` runs detached); run it from a terminal"
                );
            }
            Scope::Operator
        }
        Role::Unknown(reason) => bail!("the tasks CLI cannot identify its caller: {reason}"),
        Role::Operator => Scope::Operator,
        Role::Chat { origin, chat } => {
            if matches!(cmd, Cmd::Sweep) {
                bail!("`tasks sweep` is maintenance; a chat session cannot run it");
            }
            Scope::Origin { origin: origin.clone(), origin_ref: chat.clone() }
        }
    };
    if caller.inbound_hop > 0 && matches!(cmd, Cmd::Start { .. } | Cmd::Cancel { .. }) {
        bail!(
            "this turn is reacting to a message from another agent (hop:{}); starting or \
             cancelling a task needs a request from the operator — ask the operator",
            caller.inbound_hop
        );
    }
    Ok(scope)
}

/// Entry point for `nucleus tasks`. `args` includes argv[0].
pub async fn run(args: Vec<std::ffi::OsString>) -> Result<()> {
    let cli = Cli::parse_from(args);
    let settings = crate::config::Settings::load().context("loading settings")?;
    let ws = match cli.workspace_root {
        Some(p) => p,
        None => settings.workspace_root()?,
    };

    if let Cmd::Run { id } = &cli.cmd {
        // The detached worker itself: `launch_worker` wrote its one-time run
        // token to stdin. Without the token the task is refused.
        let mut token = String::new();
        std::io::stdin().read_line(&mut token).context("reading the run token")?;
        return tasks::run_worker(&settings, &ws, id, &token).await;
    }

    let caller = caller::detect(&ws).await?;
    let scope = authorize(&caller, &cli.cmd)?;

    let pool = tasks::open(&ws).await?;
    // Every operator-facing command first settles tasks whose worker died
    // and retries unfinished deliveries, so what it prints is true.
    let swept = tasks::sweep(&ws, &pool, &settings.tasks).await?;

    match cli.cmd {
        Cmd::Start { title, brief, brief_file, origin, origin_ref, requested_by, kind, parent, links } => {
            let (origin, origin_ref) = match &scope {
                Scope::Operator => (origin.unwrap_or_else(|| "cli".into()), origin_ref),
                Scope::Origin { origin: o, origin_ref: r } => {
                    if origin.as_deref().is_some_and(|x| x != o) || origin_ref.as_deref().is_some_and(|x| x != r) {
                        bail!("the origin of a chat session's task is its own chat; drop --origin/--origin-ref");
                    }
                    (o.clone(), Some(r.clone()))
                }
            };
            if matches!(scope, Scope::Origin { .. }) && !matches!(requested_by.as_str(), "operator" | "model") {
                bail!("--requested-by must be operator or model");
            }
            let brief = match (brief, brief_file) {
                (Some(b), _) if b == "-" => {
                    let mut s = String::new();
                    std::io::stdin().read_to_string(&mut s).context("reading the brief from stdin")?;
                    s
                }
                (Some(b), _) => b,
                (None, Some(f)) => std::fs::read_to_string(&f)
                    .with_context(|| format!("reading {}", f.display()))?,
                (None, None) => bail!("give the brief with --brief <text>, --brief - or --brief-file <path>"),
            };
            let links = links
                .iter()
                .map(|l| {
                    l.split_once('=')
                        .map(|(r, t)| (r.trim().to_string(), t.trim().to_string()))
                        .with_context(|| format!("--link {l:?} must look like rel=target"))
                })
                .collect::<Result<Vec<_>>>()?;
            let task = tasks::create(
                &pool,
                NewTask {
                    kind,
                    title,
                    brief,
                    origin,
                    origin_ref,
                    parent_id: parent,
                    requested_by,
                    links,
                    // The CLI never sets these: a chat session must not
                    // choose where a worker runs or relax its posture.
                    workdir: None,
                    profile: tasks::WorkerProfile::Agentic,
                },
                &scope,
            )
            .await?;
            tasks::launch_worker(&ws, &pool, &task.id).await?;
            println!(
                "started task {} — {:?}. Result goes to {} when it finishes. \
                 Check it with: nucleus tasks status {}",
                task.short_id(),
                task.title,
                task.origin,
                task.short_id()
            );
        }
        Cmd::List { all, limit, json } => {
            let mut rows = tasks::list(&pool, !all, limit, &scope).await?;
            if !all {
                let recent: Vec<Task> = tasks::list(&pool, false, 10, &scope)
                    .await?
                    .into_iter()
                    .filter(|t| t.status().is_terminal())
                    .collect();
                rows.extend(recent);
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if rows.is_empty() {
                println!("no tasks");
            } else {
                for t in &rows {
                    println!("{}", line(t));
                }
            }
        }
        Cmd::Status { id, json } => {
            let t = tasks::get(&pool, &id, &scope).await?;
            let ev = tasks::events(&pool, &t.id).await?;
            let links = tasks::links(&pool, &t.id).await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({ "task": t, "events": ev, "links": links })
                    )?
                );
            } else {
                println!("{}", line(&t));
                println!("kind {} · origin {} · requested by {}", t.kind, t.origin, t.requested_by);
                println!("created {} · started {} · finished {}",
                    t.created_at,
                    t.started_at.as_deref().unwrap_or("-"),
                    t.finished_at.as_deref().unwrap_or("-"));
                if let Some(w) = &t.tmux_window {
                    println!("session window {w} in tmux session {}", settings.tasks.tmux_session);
                }
                for l in &links {
                    println!("link {} = {}", l.rel, l.target);
                }
                if let Some(e) = &t.error {
                    println!("error: {e}");
                }
                if t.delivered_at.is_none() {
                    if let Some(at) = &t.delivery_failed_at {
                        println!(
                            "delivery given up {at} (not sent again): {}",
                            t.delivery_error.as_deref().unwrap_or("no reason recorded")
                        );
                    }
                }
                println!("\nlog:");
                for e in ev.iter().rev().take(15).collect::<Vec<_>>().into_iter().rev() {
                    println!("  {} {:<16} {}", e.at, e.kind, e.message.replace('\n', " "));
                }
            }
        }
        Cmd::Output { id } => {
            let t = tasks::get(&pool, &id, &scope).await?;
            match (&t.result, t.status().is_terminal()) {
                (Some(r), _) => println!("{r}"),
                (None, true) => println!(
                    "task {} ended as {} with no result{}",
                    t.short_id(),
                    t.status,
                    t.error.as_deref().map(|e| format!(": {e}")).unwrap_or_default()
                ),
                (None, false) => {
                    let ev = tasks::events(&pool, &t.id).await?;
                    match ev.iter().rev().find(|e| e.kind == "progress") {
                        Some(p) => println!("task {} is {}; latest progress ({}):\n{}", t.short_id(), t.status, p.at, p.message),
                        None => println!("task {} is {}; no progress text yet", t.short_id(), t.status),
                    }
                }
            }
        }
        Cmd::Cancel { id } => {
            let t = tasks::request_cancel(&ws, &pool, &settings.tasks, &id, &scope).await?;
            println!("task {} cancelled", t.short_id());
        }
        Cmd::Sweep => {
            for t in &swept {
                println!("interrupted: {}", line(t));
            }
            if swept.is_empty() {
                println!("no tasks without a worker");
            }
        }
        Cmd::Run { .. } => unreachable!(),
    }
    Ok(())
}

fn line(t: &Task) -> String {
    let dur = duration(t).map(|d| format!(" · {d}")).unwrap_or_default();
    format!("{} {:<11} {}{}", t.short_id(), t.status, t.title, dur)
}

fn duration(t: &Task) -> Option<String> {
    let start = chrono::DateTime::parse_from_rfc3339(t.started_at.as_deref()?).ok()?;
    let end = match t.finished_at.as_deref() {
        Some(f) => chrono::DateTime::parse_from_rfc3339(f).ok()?,
        None => chrono::Utc::now().fixed_offset(),
    };
    let secs = (end - start).num_seconds().max(0);
    Some(if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caller(role: Role, hop: u8) -> Caller {
        Caller { role, agent: None, session_id: None, inbound_hop: hop }
    }

    fn start() -> Cmd {
        Cmd::Start {
            title: "t".into(),
            brief: Some("b".into()),
            brief_file: None,
            origin: None,
            origin_ref: None,
            requested_by: "model".into(),
            kind: "general".into(),
            parent: None,
            links: vec![],
        }
    }

    #[test]
    fn authorization_by_caller() {
        let chat = Role::Chat { origin: "whatsapp-dm".into(), chat: "5511999999999@s.whatsapp.net".into() };
        assert_eq!(authorize(&caller(Role::Operator, 0), &start()).unwrap(), Scope::Operator);
        assert_eq!(
            authorize(&caller(chat.clone(), 0), &start()).unwrap(),
            Scope::Origin { origin: "whatsapp-dm".into(), origin_ref: "5511999999999@s.whatsapp.net".into() }
        );
        assert!(authorize(&caller(chat.clone(), 0), &Cmd::Sweep).is_err());
        assert!(authorize(&caller(chat.clone(), 0), &Cmd::Run { id: "abcd".into() }).is_err());
        assert!(authorize(&caller(Role::Worker { task_id: None }, 0), &Cmd::List { all: true, limit: 5, json: false }).is_err());
        assert!(authorize(&caller(Role::UnscopedChat, 0), &Cmd::List { all: true, limit: 5, json: false }).is_err());
        // A turn reacting to an agent message may read, not start or cancel.
        assert!(authorize(&caller(chat.clone(), 1), &start()).is_err());
        assert!(authorize(&caller(chat.clone(), 1), &Cmd::Cancel { id: "abcd".into() }).is_err());
        assert!(authorize(&caller(chat, 1), &Cmd::Status { id: "abcd".into(), json: false }).is_ok());
        assert!(authorize(&caller(Role::Operator, 1), &start()).is_err());
        // `tasks run` is never authorized by role: only the run token opens it.
        assert!(authorize(&caller(Role::Operator, 0), &Cmd::Run { id: "abcd".into() }).is_err());
        // Other Nucleus sessions, detached and unidentified processes never
        // fall back to the operator.
        let list = || Cmd::List { all: true, limit: 5, json: false };
        assert!(authorize(&caller(Role::Session { kind: "agent".into() }, 0), &start()).is_err());
        assert!(authorize(&caller(Role::Session { kind: "agent".into() }, 0), &list()).is_err());
        assert!(authorize(&caller(Role::Detached, 0), &start()).is_err());
        assert!(authorize(&caller(Role::Detached, 0), &list()).is_err());
        assert_eq!(authorize(&caller(Role::Detached, 0), &Cmd::Sweep).unwrap(), Scope::Operator);
        assert!(authorize(&caller(Role::Unknown("x".into()), 0), &list()).is_err());
        assert!(authorize(&caller(Role::Unknown("x".into()), 0), &Cmd::Sweep).is_err());
    }
}
