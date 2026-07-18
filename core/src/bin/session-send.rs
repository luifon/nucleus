//! `session-send` — the ONE sanctioned way an agent writes into another
//! agent's live Claude session (ADR-021). Thin CLI over
//! `nucleus_core::agent_msg::send`.
//!
//! Examples:
//!   session-send --to nucleus-whatsapp-dm:1 --from main \
//!     --message "context brief …"
//!   session-send --to nucleus-gmail --from reminders-fire --await-reply \
//!     --timeout 120 --message "what's the state of the inbox sweep?"

use anyhow::Result;
use clap::Parser;
use nucleus_core::agent_msg::{SendOpts, send};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Parser)]
#[command(
    name = "session-send",
    about = "Inject an attributed agent message into a registered live Claude session (ADR-021)"
)]
struct Cli {
    /// Target tmux session[:window]; the session must belong to a registered
    /// agent (agents.toml), exact or as `<registered>-suffix`.
    #[arg(long)]
    to: String,
    /// Sender agent label — written into the attribution header and the log.
    #[arg(long)]
    from: String,
    /// Message body. The `[agent-msg …]` header is machine-prepended; do not
    /// include one yourself.
    #[arg(long)]
    message: String,
    /// Hop count of the agent-msg THIS send reacts to. 0 = originating.
    /// hop:1 is terminal (ADR-021) — passing 1 here is refused.
    #[arg(long, default_value_t = 0)]
    hop: u8,
    /// Wait for the target's reply (transcript-tailed) and print it.
    #[arg(long, default_value_t = false)]
    await_reply: bool,
    /// Reply timeout in seconds (only with --await-reply).
    #[arg(long, default_value_t = 120)]
    timeout: u64,
    /// Workspace root (defaults to the current directory).
    #[arg(long)]
    workspace_root: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let cli = Cli::parse();
    let workspace_root = match cli.workspace_root {
        Some(p) => p,
        None => std::env::current_dir()?,
    };
    let report = send(SendOpts {
        to: cli.to,
        from: cli.from,
        message: cli.message,
        hop: cli.hop,
        await_reply: cli.await_reply.then(|| Duration::from_secs(cli.timeout)),
        workspace_root,
    })
    .await?;

    eprintln!("✓ delivered to {} as {}", report.target, report.header);
    if let Some(reply) = report.reply {
        println!("{reply}");
    }
    Ok(())
}
