//! skill-gap-learner — the all-facets successor to preference-learner (ADR-017).
//!
//! Two arms, one review engine, ported from Hermes' skill-review + curator:
//!   review  — on-the-fly: read ONE conversation transcript, autonomously
//!             create/patch skills it warrants. Fired detached by the
//!             conversational agents every N turns.
//!   learn   — periodic (launchd-cron): propose skills for recurring patterns
//!             across all diaries, then curate (stale/archive + consolidate).
//!
//! Autonomous writes go to `~/.claude/skills/` only (operator-personal,
//! gitignored — Rule 1). Every touched SKILL.md is run through
//! `nucleus_core::skills::validate`; a malformed write is quarantined to
//! `.rejected/` instead of polluting the live library.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nucleus_core::{
    claude::PermissionMode,
    claude_session::{last_n_turns, AskOptions, Session, SpawnOptions, TurnRole},
    config::Settings,
    diary, skills,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const AGENT_NAME: &str = "skill-gap-learner";
const TMUX_SESSION: &str = "nucleus-skill-gap-learner";

#[derive(Parser)]
#[command(name = "skill-gap-learner", about = "Nucleus skill-gap learner (ADR-017)")]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// On-the-fly: review one conversation transcript and update skills.
    Review {
        /// Path to the Claude transcript JSONL to review.
        #[arg(long)]
        transcript: PathBuf,
        /// Conversational venue the transcript came from (discord|chat|whatsapp).
        #[arg(long)]
        venue: String,
        /// Chat key (for the diary context line). Optional.
        #[arg(long)]
        chat_key: Option<String>,
    },
    /// Periodic: propose missing skills across diaries, then curate. (default)
    Learn,
}

#[tokio::main]
async fn main() -> Result<()> {
    nucleus_core::init_tracing();
    let _settings = Settings::load().context("loading settings")?;
    let workspace_root = std::env::current_dir()?;
    let cli = Cli::parse();

    match cli.command.unwrap_or(Cmd::Learn) {
        Cmd::Review { transcript, venue, chat_key } => {
            review(&workspace_root, &transcript, &venue, chat_key.as_deref()).await
        }
        Cmd::Learn => {
            // Filled in by the `learn` step (ADR-017 phase 3).
            tracing::info!("skill-gap-learner: `learn` not yet implemented");
            Ok(())
        }
    }
}

fn operator_skills_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".claude/skills")
}

/// On-the-fly review of a single conversation (the Hermes skill-review arm).
async fn review(
    workspace_root: &Path,
    transcript: &Path,
    venue: &str,
    chat_key: Option<&str>,
) -> Result<()> {
    let operator_root = operator_skills_root();
    let repo_root = workspace_root.join(".claude/skills");

    // Extract the recent conversation (clean user/assistant text — corrections,
    // frustration, techniques described). Bail cheaply if there's nothing to
    // review (a single-turn exchange rarely warrants a skill).
    let turns = last_n_turns(transcript, 50);
    if turns.len() < 3 {
        tracing::info!("review[{venue}]: transcript too short ({} turns) — skipping", turns.len());
        return Ok(());
    }
    let conversation = render_conversation(&turns);

    // Library summary so the reviewer can decide patch-vs-create.
    let mut lib = skills::read_skills(&operator_root, "personal");
    lib.extend(skills::read_skills(&repo_root, "repo"));
    let library = render_library(&lib);

    let prompt = build_review_prompt(&operator_root, &library, venue, &conversation);

    // Snapshot before, so we can find what the reviewer actually wrote.
    let started = SystemTime::now();

    let mut session = Session::spawn(SpawnOptions {
        workspace_root: workspace_root.to_path_buf(),
        permission_mode: Some(PermissionMode::Auto),
        // Let the session write into the operator-personal skills tree.
        add_dirs: vec![operator_root.clone()],
        tmux_session: TMUX_SESSION.into(),
        window_name: Some(format!("review-{venue}")),
        agent_label: Some(AGENT_NAME.into()),
        ready_timeout: Duration::from_secs(20),
        ..SpawnOptions::default()
    })
    .await
    .context("spawning skill-review session")?;

    let raw = session
        .ask(&prompt, AskOptions {
            max_wait: Duration::from_secs(300),
            quiescent_window: Duration::from_secs(5),
        })
        .await;
    let _ = session.close().await;
    let reply = raw.context("skill-review ask() failed")?;

    // Validation gate: any SKILL.md the reviewer touched must conform, or it's
    // quarantined out of the live library (ADR-017 format gate).
    let quarantined = gate_touched_skills(&operator_root, started)?;

    let summary = if quarantined.is_empty() {
        format!("reviewed {venue} ({} turns): {}", turns.len(), truncate(&reply, 240))
    } else {
        format!(
            "reviewed {venue}: quarantined {} malformed skill(s): {}",
            quarantined.len(),
            quarantined.join(", ")
        )
    };
    let ctx = match chat_key {
        Some(k) => format!("review:{venue}:{k}"),
        None => format!("review:{venue}"),
    };
    let _ = diary::record_observation(workspace_root, AGENT_NAME, &ctx, &summary, diary::Tag::Observation);
    tracing::info!("review[{venue}]: {summary}");
    Ok(())
}

/// Validate every SKILL.md under `root` modified since `since`; move any that
/// fail to `<root>/.rejected/<name>-<ts>/`. Returns the names quarantined.
fn gate_touched_skills(root: &Path, since: SystemTime) -> Result<Vec<String>> {
    let mut quarantined = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return Ok(quarantined),
    };
    for dirent in entries.flatten() {
        let dir = dirent.path();
        if !dir.is_dir() {
            continue;
        }
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        if name.starts_with('.') {
            continue; // .archive / .rejected
        }
        let skill_md = dir.join(skills::SKILL_FILE);
        let Ok(meta) = std::fs::metadata(&skill_md) else { continue };
        let Ok(modified) = meta.modified() else { continue };
        if modified < since {
            continue; // not touched this run
        }
        let content = std::fs::read_to_string(&skill_md).unwrap_or_default();
        let issues = skills::validate(&content);
        if issues.is_empty() {
            continue;
        }
        // Quarantine.
        let ts = chrono::Utc::now().format("%Y%m%dT%H%M%S");
        let rejected = root.join(".rejected").join(format!("{name}-{ts}"));
        if let Some(parent) = rejected.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::rename(&dir, &rejected) {
            Ok(_) => {
                tracing::warn!("review: quarantined `{name}` → {} ({:?})", rejected.display(), issues);
                quarantined.push(name);
            }
            Err(e) => tracing::warn!("review: failed to quarantine `{name}`: {e}"),
        }
    }
    Ok(quarantined)
}

fn render_conversation(turns: &[nucleus_core::claude_session::Turn]) -> String {
    let mut out = String::new();
    for t in turns {
        let label = match t.role {
            TurnRole::User => "USER",
            TurnRole::Assistant => "ASSISTANT",
        };
        out.push_str(label);
        out.push_str(": ");
        out.push_str(t.text.trim());
        out.push_str("\n\n");
    }
    out
}

fn render_library(lib: &[skills::Skill]) -> String {
    if lib.is_empty() {
        return "(the skill library is currently empty)".into();
    }
    let mut out = String::new();
    for s in lib {
        out.push_str(&format!("- {} [{}]: {}\n", s.name, s.tier, truncate(&s.description, 120)));
    }
    out
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max).collect();
    t.push('…');
    t
}

/// The ported Hermes SKILL_REVIEW_PROMPT, adapted for Nucleus: writes go to
/// the operator-personal tree via the session's own file tools; the SKILL.md
/// contract (frontmatter + the required `# Failure modes` section, Rule 11)
/// is spelled out so direct writes match what skill-creator would scaffold.
fn build_review_prompt(operator_root: &Path, library: &str, venue: &str, conversation: &str) -> String {
    let dir = operator_root.display();
    format!(
        r#"You are Nucleus' background skill reviewer. Review the {venue} conversation below and update the skill library. Be ACTIVE — most sessions that contain a correction or a non-trivial technique should produce at least one skill update. A pass that does nothing when a signal fired is a missed learning opportunity. But "Nothing to save." is a real and correct answer for a smooth, unremarkable exchange — say it and stop.

Target shape: CLASS-LEVEL skills, each a rich SKILL.md, not a flat list of one-session-one-skill entries.

Signals to look for (any one warrants action):
  • The user corrected your style, tone, format, verbosity, or legibility — "stop doing X", "too verbose", "just give me the answer", "you always do Y and I hate it", or an explicit "remember this". Embed the preference in the relevant skill so the next session starts knowing it.
  • The user corrected your workflow, approach, or sequence of steps. Encode it as a pitfall or explicit step.
  • A non-trivial technique, fix, workaround, debugging path, or tool-usage pattern emerged that a future session would benefit from.
  • A skill that was loaded/consulted this session turned out wrong, missing a step, or outdated. Patch it NOW.

Preference order — pick the earliest that fits when a signal fired:
  1. PATCH a skill that was loaded/consulted this session (the one in play).
  2. PATCH an existing class-level skill that covers the territory (add a subsection, pitfall, or broaden a trigger).
  3. ADD a support file under an existing skill: references/<topic>.md (session detail, quirks), templates/<name>.<ext> (copy-and-modify starters), or scripts/<name>.<ext> (re-runnable actions). Add a one-line pointer in the SKILL.md.
  4. CREATE a new class-level skill ONLY when no existing skill covers the class. The name MUST be class-level — never a PR number, error string, codename, or "fix-X-today" artifact.

Do NOT capture (these harden into self-imposed constraints that bite later):
  • Environment-dependent failures (missing binary, "command not found", unconfigured credential, post-migration path). The operator fixes these — capture the FIX under a setup skill if anything, never "X doesn't work".
  • Negative claims about tools ("browser tools don't work", "Y is broken"). They become refusals the bot cites for months.
  • Transient errors that resolved before the conversation ended (capture the retry pattern, not the original failure).
  • One-off task narratives.

HOW TO WRITE (this is Nucleus, not Hermes — there is no skill_manage tool):
  • Write skills with your own Read/Write/Edit file tools, under {dir}/<skill-name>/SKILL.md (operator-personal tree ONLY — never the repo's .claude/skills).
  • Every CREATED skill's SKILL.md MUST have this exact shape or it will be rejected by the validator:
      ---
      name: <kebab-case-class-level-name>
      description: <one line — what class of task + when>
      flavor: learned
      created_by: agent
      last_used: {today}
      ---

      # When to invoke
      <natural-language triggers>

      # Steps
      <ordered procedure>

      # Failure modes
      <what goes wrong + how to recover — REQUIRED, never empty>
  • When you PATCH an existing skill, bump its `last_used: {today}` and keep the required sections intact.
  • Never write a secret, token, phone number, email, or personal identifier into a skill body.

Current skill library:
{library}

Conversation to review ({venue}):
---
{conversation}
---

Do the work now, then reply with a ONE-LINE summary of what you changed (e.g. "patched git-rebase-recovery: added the detached-HEAD pitfall") or exactly "Nothing to save."."#,
        today = chrono::Local::now().format("%Y-%m-%d"),
    )
}
