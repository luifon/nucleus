//! Nucleus shared library.
//!
//! Modules:
//! - [`agents`] — the agent registry loaded from `agents.toml` (see ADR-016).
//! - [`claude`] — shared `PermissionMode` enum.
//! - [`claude_session`] — long-lived interactive `claude` sessions driven via
//!   tmux.
//! - [`config`] — typed settings loaded from `nucleus.toml` + env.
//! - [`db`] — sqlx pool helpers.
//! - [`diary`] — Tier 1.5 per-agent journals (see ADR-004).
//! - [`discord_sdk`] — outbound Discord helpers (S1).
//! - [`health`] — `Snapshot`/`Status` health wire types.
//! - [`memory`] — Tier 2 shared-fact read/write (see ADR-002).
//! - [`runlog`] — per-agent run-log index over Claude transcripts (ADR-016).
//! - [`skills`] — shared SKILL.md discovery/parse/validate (ADR-008/017).
//! - [`timestamp`] — the canonical sortable text form for stored timestamps.
//! - [`usage`] — token and estimated-cost accounting over transcripts (ADR-034).
//! - [`vault`] — vault search index and vault check over the Obsidian vault (ADR-035).

pub mod agent_msg;
pub mod agents;
pub mod caller;
pub mod chore_state;
pub mod claude;
pub mod claude_session;
pub mod cmd;
pub mod config;
pub mod db;
pub mod diary;
pub mod discord_sdk;
pub mod health;
pub mod intake;
pub mod memory;
pub mod migrate;
pub mod proc_tree;
pub mod runlog;
pub mod secret_filter;
pub mod session_index;
pub mod session_profile;
pub mod skills;
pub mod tasks;
pub mod timestamp;
pub mod usage;
pub mod vault;
pub mod turn_tracker;
pub mod whatsapp_queue;

pub fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("NUCLEUS_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).try_init();
}

/// Tracing for operator tools whose stdout is their output (`tasks`,
/// `session-send`, `session-search`): log lines go to stderr so a caller
/// parsing stdout (a chat session reading `tasks list --json`) gets only the
/// result.
pub fn init_tracing_stderr() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("NUCLEUS_LOG")
        .unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
}
