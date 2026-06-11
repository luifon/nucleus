//! Session profiles (ADR-020) — the one place session-spawn posture lives.
//!
//! Before this module, every binary hand-rolled its own
//! `SpawnOptions`/`AskOptions` literal: 5 distinct shapes across 7 call
//! sites, and twice the safety-critical knobs got silently dropped
//! (skill-gap-learner ran with `await_turn_complete: false` and without the
//! Settings `disallowed_tools`). Profiles make those mistakes
//! unrepresentable:
//!
//! - Every one-shot constructor hard-codes `await_turn_complete: true` —
//!   correct for agentic multi-step work, and also *faster* for single-turn
//!   utility asks (`end_turn` returns immediately instead of waiting out the
//!   quiescence window). There is deliberately no override for it.
//! - `permission_mode` + `disallowed_tools` come from [`ProfileContext`],
//!   which borrows the Settings-derived [`ClaudeConfig`] — a call site
//!   cannot reach `Session::spawn` through a profile without the configured
//!   security posture. There's an `extend_disallowed_tools` (add-only), but
//!   no setter.
//! - MCP-gated sessions take their `allowed_tools` as a constructor
//!   argument, so "gated" can't be constructed ungated.
//!
//! Interactive pools keep `await_turn_complete: false` via
//! [`interactive_pool`] — short conversational turns where the legacy
//! quiescent behavior is right and a hung tool shouldn't block `max_wait`.

use crate::claude::PermissionMode;
use crate::claude_session::{
    AskOptions, PoolConfig, Session, SpawnOptions, transcript_ends_with_clean_reply_async,
};
use crate::config::ClaudeConfig;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Everything a profile derives from the environment. Borrowed so call
/// sites can't forget the Settings-derived posture.
pub struct ProfileContext<'a> {
    pub workspace_root: &'a Path,
    /// `settings.claude` — permission_mode + disallowed_tools.
    pub claude: &'a ClaudeConfig,
    /// Venue-based tmux session name (Rule 7), e.g. `nucleus-gmail`.
    pub tmux_session: &'a str,
    /// Registry agent name for the run-log (ADR-016).
    pub agent_label: &'a str,
}

/// A paired `(SpawnOptions, AskOptions)` with profile-enforced invariants.
pub struct SessionProfile {
    spawn: SpawnOptions,
    ask: AskOptions,
}

fn resolve_permission_mode(claude: &ClaudeConfig) -> Option<PermissionMode> {
    match PermissionMode::parse(&claude.permission_mode) {
        Some(m) => Some(m),
        None => {
            tracing::warn!(
                mode = %claude.permission_mode,
                "unknown claude permission_mode in config — falling back to auto"
            );
            Some(PermissionMode::Auto)
        }
    }
}

fn base_spawn(ctx: &ProfileContext) -> SpawnOptions {
    SpawnOptions {
        workspace_root: ctx.workspace_root.to_path_buf(),
        append_system_prompt: None,
        permission_mode: resolve_permission_mode(ctx.claude),
        disallowed_tools: ctx.claude.disallowed_tools.clone(),
        allowed_tools: vec![],
        add_dirs: vec![],
        tmux_session: ctx.tmux_session.to_string(),
        window_name: None,
        ready_timeout: Duration::from_secs(20),
        resume_session_id: None,
        agent_label: Some(ctx.agent_label.to_string()),
    }
}

impl SessionProfile {
    /// One-shot, no tools expected (JSON scoring, title generation,
    /// distiller passes). 180s ceiling, 3s quiescence, end_turn-gated.
    pub fn one_shot_utility(ctx: &ProfileContext) -> Self {
        Self {
            spawn: base_spawn(ctx),
            ask: AskOptions {
                max_wait: Duration::from_secs(180),
                quiescent_window: Duration::from_secs(3),
                await_turn_complete: true,
            },
        }
    }

    /// One-shot, multi-step tool-using task (skill fires, skill writing).
    /// 300s ceiling, 5s quiescence, end_turn-gated — the model goes quiet
    /// between tool calls; quiescence alone tears it down mid-task (the
    /// DSU skill-fire failure mode, 2026-05-26).
    pub fn one_shot_agentic(ctx: &ProfileContext) -> Self {
        Self {
            spawn: base_spawn(ctx),
            ask: AskOptions {
                max_wait: Duration::from_secs(300),
                quiescent_window: Duration::from_secs(5),
                await_turn_complete: true,
            },
        }
    }

    /// One-shot gated on specific MCP tools (gmail metabolism, calendar
    /// fire). = agentic + `--allowed-tools` pre-approval. The list is a
    /// required argument, not an override, so "MCP-gated" can't be
    /// constructed ungated.
    pub fn one_shot_mcp(ctx: &ProfileContext, allowed_tools: Vec<String>) -> Self {
        let mut p = Self::one_shot_agentic(ctx);
        p.spawn.allowed_tools = allowed_tools;
        p
    }

    // ── overrides (chainable). Deliberately absent: await_turn_complete,
    //    and any disallowed_tools *setter* (only the add-only extend). ──

    /// Persona / instructions appended to the system prompt.
    pub fn system_prompt(mut self, p: impl Into<String>) -> Self {
        self.spawn.append_system_prompt = Some(p.into());
        self
    }

    pub fn window_name(mut self, n: impl Into<String>) -> Self {
        self.spawn.window_name = Some(n.into());
        self
    }

    pub fn add_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.spawn.add_dirs = dirs;
        self
    }

    pub fn max_wait(mut self, d: Duration) -> Self {
        self.ask.max_wait = d;
        self
    }

    pub fn quiescent_window(mut self, d: Duration) -> Self {
        self.ask.quiescent_window = d;
        self
    }

    pub fn ready_timeout(mut self, d: Duration) -> Self {
        self.spawn.ready_timeout = d;
        self
    }

    /// Add tool patterns on top of the Settings denylist (never replaces it).
    pub fn extend_disallowed_tools(mut self, extra: Vec<String>) -> Self {
        self.spawn.disallowed_tools.extend(extra);
        self
    }

    /// Escape hatch for call sites that manage the session lifecycle
    /// themselves (e.g. the distiller reuses one session across many asks).
    /// The invariants baked into the options still hold.
    pub fn into_parts(self) -> (SpawnOptions, AskOptions) {
        (self.spawn, self.ask)
    }

    /// spawn → ask → close, with the forensics handles a caller needs
    /// afterwards. The standard shape for every one-shot fire.
    pub async fn run_one_shot(self, message: &str) -> Result<OneShotOutcome> {
        let label = self.spawn.agent_label.clone().unwrap_or_default();
        let mut session = Session::spawn(self.spawn)
            .await
            .with_context(|| format!("spawning one-shot session ({label})"))?;
        let raw = session.ask(message, self.ask).await;
        let session_id = session.session_id().to_string();
        // Capture before close(); the transcript persists after the tmux
        // window dies, so the caller can inspect how the session ended.
        let transcript_path = session.transcript_path().to_path_buf();
        let _ = session.close().await;
        let reply = raw.with_context(|| format!("one-shot ask() failed ({label})"))?;
        let ended_clean = transcript_ends_with_clean_reply_async(&transcript_path).await;
        Ok(OneShotOutcome { reply, session_id, transcript_path, ended_clean })
    }
}

/// Outcome of [`SessionProfile::run_one_shot`].
pub struct OneShotOutcome {
    pub reply: String,
    pub session_id: String,
    pub transcript_path: PathBuf,
    /// `transcript_ends_with_clean_reply` — the narration-leak guard.
    /// Callers forwarding unattended output to an audience MUST check
    /// this: a session cut off mid-action hands back a stale internal
    /// line ("Let me click…") as `reply`, and forwarding that posts the
    /// model's monologue under the operator's identity (a DSU fire,
    /// GH 2026-05-25 + FS 2026-05-26).
    pub ended_clean: bool,
}

/// Pool profile for interactive conversational venues (Discord, dashboard
/// chat): returns the `PoolConfig` plus the `AskOptions` every pool turn
/// should use — `await_turn_complete: false`, since these are short
/// interactive turns where quiescence is the right doneness signal and a
/// hung tool shouldn't block up to `max_wait`.
pub fn interactive_pool(
    ctx: &ProfileContext,
    persona: String,
    idle_timeout: Duration,
    review_nudge_interval: u32,
) -> (PoolConfig, AskOptions) {
    (
        PoolConfig {
            workspace_root: ctx.workspace_root.to_path_buf(),
            append_system_prompt: Some(persona),
            permission_mode: resolve_permission_mode(ctx.claude),
            disallowed_tools: ctx.claude.disallowed_tools.clone(),
            allowed_tools: vec![],
            add_dirs: vec![],
            tmux_session: ctx.tmux_session.to_string(),
            idle_timeout,
            agent_label: Some(ctx.agent_label.to_string()),
            review_nudge_interval,
        },
        AskOptions {
            max_wait: Duration::from_secs(180),
            quiescent_window: Duration::from_secs(3),
            await_turn_complete: false,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_fixture() -> (PathBuf, ClaudeConfig) {
        (
            PathBuf::from("/tmp/ws"),
            ClaudeConfig {
                binary: "claude".into(),
                permission_mode: "auto".into(),
                disallowed_tools: vec!["Bash(rm *)".into()],
            },
        )
    }

    #[test]
    fn one_shots_are_end_turn_gated_and_carry_the_denylist() {
        let (ws, claude) = ctx_fixture();
        let ctx = ProfileContext {
            workspace_root: &ws,
            claude: &claude,
            tmux_session: "nucleus-test",
            agent_label: "test",
        };
        for profile in [
            SessionProfile::one_shot_utility(&ctx),
            SessionProfile::one_shot_agentic(&ctx),
            SessionProfile::one_shot_mcp(&ctx, vec!["mcp__x__y".into()]),
        ] {
            let (spawn, ask) = profile.into_parts();
            assert!(ask.await_turn_complete, "one-shots must be end_turn-gated");
            assert_eq!(spawn.disallowed_tools, vec!["Bash(rm *)".to_string()]);
            assert_eq!(spawn.permission_mode, Some(PermissionMode::Auto));
            assert_eq!(spawn.agent_label.as_deref(), Some("test"));
        }
    }

    #[test]
    fn extend_disallowed_tools_is_add_only() {
        let (ws, claude) = ctx_fixture();
        let ctx = ProfileContext {
            workspace_root: &ws,
            claude: &claude,
            tmux_session: "nucleus-test",
            agent_label: "test",
        };
        let (spawn, _) = SessionProfile::one_shot_utility(&ctx)
            .extend_disallowed_tools(vec!["Bash(mv *)".into()])
            .into_parts();
        assert_eq!(spawn.disallowed_tools.len(), 2);
        assert!(spawn.disallowed_tools.contains(&"Bash(rm *)".to_string()));
    }

    #[test]
    fn unknown_permission_mode_falls_back_to_auto() {
        let (ws, mut claude) = ctx_fixture();
        claude.permission_mode = "yolo".into();
        let ctx = ProfileContext {
            workspace_root: &ws,
            claude: &claude,
            tmux_session: "nucleus-test",
            agent_label: "test",
        };
        let (spawn, _) = SessionProfile::one_shot_utility(&ctx).into_parts();
        assert_eq!(spawn.permission_mode, Some(PermissionMode::Auto));
    }

    #[test]
    fn interactive_pool_keeps_quiescent_asks() {
        let (ws, claude) = ctx_fixture();
        let ctx = ProfileContext {
            workspace_root: &ws,
            claude: &claude,
            tmux_session: "nucleus-test",
            agent_label: "test",
        };
        let (cfg, ask) = interactive_pool(&ctx, "persona".into(), Duration::from_secs(60), 0);
        assert!(!ask.await_turn_complete);
        assert_eq!(cfg.disallowed_tools, vec!["Bash(rm *)".to_string()]);
    }
}
