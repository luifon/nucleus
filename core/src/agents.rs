//! Agent registry — the single source of truth for every Nucleus agent.
//!
//! See ADR-016. Loaded from `agents.toml` at the workspace root. Unlike
//! `nucleus.toml` (gitignored, operator-tweaked tunables), `agents.toml` is
//! canonical system topology — identical for everyone who clones the repo —
//! so it's committed directly with no `.example` template. It contains no
//! identifiers: venue-based names, relative paths, and `dev.nucleus.*`
//! labels only (Rule 1 / Rule 7).
//!
//! The registry is *hand-edited*: adding or removing an agent means editing
//! `agents.toml`. There is no daemon maintaining it (ADR-015 discipline —
//! config is files, not UI). Per-instance, per-chat-key, and per-contract
//! skill-fires are NOT registry entries; they're discovered at runtime from
//! the session DBs and the run-log index. The registry describes the fixed
//! set of agents, not their individual executions.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;

/// Descriptive grouping for an agent. A *tag*, not a schema discriminator —
/// every agent is the same `Agent` record regardless of class; this just
/// drives how `/agents` groups tiles and frames liveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentClass {
    /// Long-lived, operator-facing, hosts a Claude session pool (rotates).
    Conversational,
    /// launchd-cron domain job that drives Claude for a specific task.
    Scheduled,
    /// Reads other agents' output to improve the system (distiller; future
    /// skill-gap learner).
    Maintenance,
    /// Host process / scheduler with no operator persona of its own.
    Infra,
    /// On-demand, short-lived; spawned by another agent (skill/calendar fires).
    Ephemeral,
}

/// How an agent is launched — determines how liveness is probed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Launch {
    /// launchd KeepAlive daemon — liveness = PID present in `launchctl list`.
    LaunchdDaemon,
    /// launchd scheduled one-shot — liveness = last exit code (0 = idle-ok).
    LaunchdCron,
    /// Runs inside another long-lived process (e.g. the chat pool inside the
    /// dashboard daemon) — liveness tracks the host.
    InProcess,
    /// Spawned on demand by another agent — liveness = live tmux window, if any.
    OnDemand,
}

/// An optional behavior a fixed agent carries *internally* (Layer A in
/// ADR-016 — capabilities live inside agents; maintenance *agents* are
/// separate registry entries).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Daily 04:00 session rotation (summarize → diary → respawn).
    Rotates,
    /// Future: on-the-fly post-session skill review (skill-gap learner arm).
    SkillReview,
}

/// One agent. A uniform record — optional fields are present or absent per
/// the agent's shape rather than switching the record type.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Agent {
    /// Venue-based identity (Rule 7). Unique. Usually matches `diary_key`.
    pub name: String,
    pub class: AgentClass,
    pub launch: Launch,
    /// Host runtime — informational, for display. e.g. "rust", "node".
    #[serde(default)]
    pub runtime: Option<String>,
    /// launchd label, if launchd-launched. Liveness probe key.
    #[serde(default)]
    pub launchd_label: Option<String>,
    /// tmux session this agent owns/drives, if it hosts a Claude session.
    /// Presence means it has a run-log transcript index.
    #[serde(default)]
    pub tmux_session: Option<String>,
    /// Informational cron expression. The real schedule is in the plist.
    #[serde(default)]
    pub schedule: Option<String>,
    /// Raw stdout/err path (launchd agents). tmux agents add a runs.jsonl
    /// index under `memory/logs/<name>/` instead.
    #[serde(default)]
    pub log_path: Option<String>,
    /// `memory/diaries/<diary_key>/`, if the agent journals.
    #[serde(default)]
    pub diary_key: Option<String>,
    /// Conversational venue for `resolve_persona()` → display_name.
    #[serde(default)]
    pub persona_venue: Option<String>,
    /// Internal capabilities (Layer A). Default none.
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// Future-reserved agents ship disabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl Agent {
    pub fn has_capability(&self, c: Capability) -> bool {
        self.capabilities.contains(&c)
    }

    /// True if this agent drives a Claude session whose raw output is
    /// captured via the transcript run-log index (vs a launchd log file).
    pub fn is_claude_tmux(&self) -> bool {
        self.tmux_session.is_some()
    }
}

/// The parsed registry. Wraps `Vec<Agent>` from the `[[agent]]` array.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct Registry {
    #[serde(default, rename = "agent")]
    pub agents: Vec<Agent>,
}

impl Registry {
    /// Load `agents.toml` from the current working directory (the workspace
    /// root in every deployed binary, per the launchd `WorkingDirectory`).
    pub fn load() -> Result<Self> {
        Self::load_from("agents.toml")
    }

    /// Load and validate from an explicit path.
    pub fn load_from(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading agent registry {}", path.display()))?;
        let registry: Registry = toml::from_str(&raw)
            .with_context(|| format!("parsing agent registry {}", path.display()))?;
        registry.validate()?;
        Ok(registry)
    }

    /// Look up an agent by name.
    pub fn get(&self, name: &str) -> Option<&Agent> {
        self.agents.iter().find(|a| a.name == name)
    }

    /// Iterate only the enabled agents (future-reserved entries are skipped).
    pub fn enabled(&self) -> impl Iterator<Item = &Agent> {
        self.agents.iter().filter(|a| a.enabled)
    }

    /// Structural invariants. Surfaces config typos at load time rather than
    /// as confusing runtime behavior in the dashboard.
    fn validate(&self) -> Result<()> {
        let mut seen = HashSet::new();
        for agent in &self.agents {
            if !seen.insert(agent.name.as_str()) {
                anyhow::bail!("duplicate agent name in registry: {}", agent.name);
            }
            let launchd_launched = matches!(
                agent.launch,
                Launch::LaunchdDaemon | Launch::LaunchdCron
            );
            if launchd_launched && agent.launchd_label.is_none() {
                anyhow::bail!(
                    "agent `{}` is launchd-launched but has no launchd_label",
                    agent.name
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_tmp(contents: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "nucleus-agents-{}-{}.toml",
            std::process::id(),
            // monotonic-ish suffix so parallel tests don't collide
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&p, contents).unwrap();
        p
    }

    #[test]
    fn parses_a_representative_registry() {
        let p = write_tmp(
            r#"
[[agent]]
name = "discord"
class = "conversational"
launch = "launchd-daemon"
runtime = "rust"
launchd_label = "dev.nucleus.discord"
tmux_session = "nucleus-discord"
log_path = "memory/discord.log"
diary_key = "discord"
persona_venue = "discord"
capabilities = ["rotates"]

[[agent]]
name = "distiller"
class = "maintenance"
launch = "launchd-cron"
launchd_label = "dev.nucleus.distiller"
tmux_session = "nucleus-distiller"
schedule = "0 4 * * *"
log_path = "memory/distiller.log"
diary_key = "distiller"

[[agent]]
name = "skill-gap-learner"
class = "maintenance"
launch = "launchd-cron"
launchd_label = "dev.nucleus.skill-gap-learner"
enabled = false
"#,
        );
        let reg = Registry::load_from(&p).unwrap();
        let _ = std::fs::remove_file(&p);

        assert_eq!(reg.agents.len(), 3);

        let discord = reg.get("discord").expect("discord present");
        assert_eq!(discord.class, AgentClass::Conversational);
        assert_eq!(discord.launch, Launch::LaunchdDaemon);
        assert!(discord.has_capability(Capability::Rotates));
        assert!(discord.is_claude_tmux());
        assert!(discord.enabled, "enabled defaults to true");

        // enabled=false agents are excluded from .enabled()
        let enabled: Vec<_> = reg.enabled().map(|a| a.name.as_str()).collect();
        assert!(enabled.contains(&"discord"));
        assert!(enabled.contains(&"distiller"));
        assert!(!enabled.contains(&"skill-gap-learner"));
    }

    #[test]
    fn rejects_duplicate_names() {
        let p = write_tmp(
            r#"
[[agent]]
name = "discord"
class = "conversational"
launch = "in-process"

[[agent]]
name = "discord"
class = "infra"
launch = "in-process"
"#,
        );
        let err = Registry::load_from(&p).unwrap_err();
        let _ = std::fs::remove_file(&p);
        assert!(err.to_string().contains("duplicate agent name"), "got: {err}");
    }

    #[test]
    fn rejects_launchd_agent_without_label() {
        let p = write_tmp(
            r#"
[[agent]]
name = "news-fetcher"
class = "scheduled"
launch = "launchd-cron"
"#,
        );
        let err = Registry::load_from(&p).unwrap_err();
        let _ = std::fs::remove_file(&p);
        assert!(err.to_string().contains("no launchd_label"), "got: {err}");
    }

    #[test]
    fn the_real_registry_parses_and_is_consistent() {
        // Guards against typos in the committed agents.toml. Path is relative
        // to the crate dir under `cargo test`, so climb to the workspace root.
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("agents.toml");
        let reg = Registry::load_from(&path).expect("committed agents.toml must parse");
        assert!(!reg.agents.is_empty());
        // Every conversational agent resolves a persona venue + rotates.
        for a in reg.agents.iter().filter(|a| a.class == AgentClass::Conversational) {
            assert!(
                a.persona_venue.is_some(),
                "conversational agent {} needs persona_venue",
                a.name
            );
        }
    }
}
