//! `nucleus` — the single binary every Nucleus service runs as (ADR-030).
//!
//! There used to be nine executables. macOS ties a privacy grant to the exact
//! code hash of the binary it was given to, so each `cargo build --release`
//! invalidated every grant and the next unattended run re-prompted — on
//! 2026-09-09 that left the distiller blocked on a dialog for five hours while
//! the operator slept. One binary, signed with a stable identity
//! (`tools/build.sh`), means one grant that survives rebuilds.
//!
//! Each subcommand is a library crate exposing `run(args)`. This file only
//! dispatches: it takes argv, hands the tail to the matching crate, and does
//! nothing else. Service behaviour belongs in the service crate.

use anyhow::{Result, bail};
use std::ffi::OsString;

const USAGE: &str = "\
nucleus — Nucleus services and operator tools

Usage: nucleus <command> [args...]

Services (run by launchd):
  distiller             daily diary distillation pass
  reminders <sub>       time-triggered notifications (due|add|list|show|…)
  skill-gap-learner     skill review + curator (learn|review)
  discord               Discord bot daemon
  gmail-metabolism      daily inbox sweep
  news-fetcher          RSS pull + notability scoring
  dashboard             web dashboard + API

Operator tools:
  session-search        FTS5 search over session transcripts
  session-send          send a message into another agent session

Every command accepts --help.
";

/// Reject arguments for a subcommand that accepts none, and answer `--help`
/// instead of starting the service.
fn no_args(name: &str, sub: &[OsString], what: &str) -> Result<()> {
    let extra: Vec<String> = sub.iter().skip(1).map(|a| a.to_string_lossy().into_owned()).collect();
    if extra.is_empty() {
        return Ok(());
    }
    if extra.iter().any(|a| a == "-h" || a == "--help") {
        println!("nucleus {name} — {what}\n\nTakes no arguments.");
        std::process::exit(0);
    }
    bail!("nucleus {name} takes no arguments (got: {})", extra.join(" "));
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut argv: Vec<OsString> = std::env::args_os().collect();
    if argv.len() < 2 {
        print!("{USAGE}");
        std::process::exit(2);
    }
    let cmd = argv.remove(1);
    let name = cmd.to_string_lossy().to_string();

    // Rebuild argv for the subcommand as `nucleus <cmd>` + the rest, so clap's
    // usage line reads the way the operator typed it.
    let mut sub: Vec<OsString> = vec![OsString::from(format!("nucleus {name}"))];
    sub.extend(argv.into_iter().skip(1));

    nucleus_core::init_tracing();

    match name.as_str() {
        // Services that take no arguments. Without this guard a typo or a
        // `--help` silently STARTS them: `nucleus distiller --help` ran a full
        // distillation pass, because the crate ignores argv.
        "distiller" => {
            no_args(&name, &sub, "run one daily diary distillation pass")?;
            distiller::run(sub).await
        }
        "discord" => {
            no_args(&name, &sub, "run the Discord bot daemon")?;
            discord::run(sub).await
        }
        "news-fetcher" => {
            no_args(&name, &sub, "pull RSS and score notability")?;
            news_fetcher::run(sub).await
        }
        "dashboard" => {
            no_args(&name, &sub, "serve the web dashboard and its API")?;
            nucleus_dashboard::run(sub).await
        }
        "reminders" => reminders::run(sub).await,
        "skill-gap-learner" => skill_gap_learner::run(sub).await,
        "gmail-metabolism" => gmail::run(sub).await,
        "session-search" => nucleus_core::cmd::session_search::run(sub).await,
        "session-send" => nucleus_core::cmd::session_send::run(sub).await,
        "-h" | "--help" | "help" => {
            print!("{USAGE}");
            Ok(())
        }
        other => {
            eprint!("{USAGE}");
            bail!("unknown command: {other}");
        }
    }
}
