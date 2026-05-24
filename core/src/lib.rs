//! Nucleus shared library.
//!
//! Modules:
//! - [`agents`] — the agent registry loaded from `agents.toml` (see ADR-016).
//! - [`claude`] — shared `PermissionMode` enum.
//! - [`claude_session`] — long-lived interactive `claude` sessions driven via
//!   tmux. The way to run claude under the Max subscription — `-p` headless
//!   mode is API-only.
//! - [`config`] — typed settings loaded from `nucleus.toml` + env.
//! - [`db`] — sqlx pool helpers.
//! - [`diary`] — Tier 1.5 per-agent journals (see ADR-004).
//! - [`discord_sdk`] — outbound Discord helpers (S1).
//! - [`health`] — `HealthCheck` trait + registry (S3).
//! - [`memory`] — Tier 2 shared-fact read/write (see ADR-002).

pub mod agents;
pub mod claude;
pub mod claude_session;
pub mod config;
pub mod db;
pub mod diary;
pub mod discord_sdk;
pub mod health;
pub mod mem0;
pub mod memory;

pub fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("NUCLEUS_LOG")
        .unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = fmt().with_env_filter(filter).try_init();
}
