//! gmail-metabolism — daily JARVIS-driven inbox sweep (ADR-007).
//!
//! Subcommands:
//!   metabolize          (default) — run the daily sweep
//!   killlist add <e>    — add a sender to the auto-trash kill-list
//!   killlist list       — print kill-list contents
//!
//! Access to Gmail goes through Claude.ai's MCP servers (`mcp__claude_ai_Gmail__*`),
//! reached by spawning a Gmail-venue `Session` whose persona is resolved at
//! startup via `NUCLEUS_PERSONA_GMAIL=<slug>` → `personas/<slug>.md` (ADR-009).
//! No raw Google API client is pulled in — the session inherits MCP auth from
//! the operator's Claude Max integrations.

use anyhow::Result;

mod store;
mod metabolize;
mod cli;

#[tokio::main]
async fn main() -> Result<()> {
    nucleus_core::init_tracing();
    cli::run().await
}
