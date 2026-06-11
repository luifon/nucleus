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
    claude_session::{last_n_turns, TurnRole},
    config::Settings,
    diary, skills,
    session_profile::{ProfileContext, SessionProfile},
};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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
    let settings = Settings::load().context("loading settings")?;
    let workspace_root = std::env::current_dir()?;
    let cli = Cli::parse();

    // A stale tmux session left from a prior crash blocks `new-window`.
    let _ = tokio::process::Command::new("tmux")
        .args(["kill-session", "-t", TMUX_SESSION])
        .output()
        .await;

    match cli.command.unwrap_or(Cmd::Learn) {
        Cmd::Review { transcript, venue, chat_key } => {
            review(&workspace_root, &settings, &transcript, &venue, chat_key.as_deref()).await
        }
        Cmd::Learn => learn(&workspace_root, &settings).await,
    }
}

fn operator_skills_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".claude/skills")
}

/// On-the-fly review of a single conversation (the Hermes skill-review arm).
async fn review(
    workspace_root: &Path,
    settings: &Settings,
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
    let (reply, quarantined) = run_skill_session(
        workspace_root,
        settings,
        &operator_root,
        &format!("review-{venue}"),
        &prompt,
    )
    .await?;

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

/// Spawn a one-shot skill-writing session, run the prompt, then run the
/// validation gate over anything it touched. Shared by review / gap / curate.
/// Returns (reply, quarantined skill names).
///
/// ADR-020: goes through `SessionProfile::one_shot_agentic`, which fixes two
/// long-standing config drops by construction — this path used to run with
/// `await_turn_complete: false` (mid-task cutoff risk while writing skill
/// files) and without the Settings `disallowed_tools` denylist.
async fn run_skill_session(
    workspace_root: &Path,
    settings: &Settings,
    operator_root: &Path,
    window: &str,
    prompt: &str,
) -> Result<(String, Vec<String>)> {
    let started = SystemTime::now();
    let outcome = SessionProfile::one_shot_agentic(&ProfileContext {
        workspace_root,
        claude: &settings.claude,
        tmux_session: TMUX_SESSION,
        agent_label: AGENT_NAME,
    })
    .add_dirs(vec![operator_root.to_path_buf()])
    .window_name(window)
    .run_one_shot(prompt)
    .await
    .with_context(|| format!("skill session ({window})"))?;
    let quarantined = gate_touched_skills(operator_root, started)?;
    Ok((outcome.reply, quarantined))
}

// ── periodic arm: gap detection + curator ──────────────────────────────────

/// The periodic pass (launchd-cron). Pure auto-archive of stale agent-created
/// skills, then an LLM gap-detection pass over recent diaries, then an LLM
/// curator/consolidation pass. Mirrors the distiller's two-phase shape.
async fn learn(workspace_root: &Path, settings: &Settings) -> Result<()> {
    let operator_root = operator_skills_root();
    let repo_root = workspace_root.join(".claude/skills");
    let cfg = &settings.skill_learner;

    // 1. Pure auto-transitions (no LLM): archive agent-created, unpinned skills
    // idle past archive_after_days; count the merely-stale for the log.
    let (stale, archived) =
        apply_auto_transitions(&operator_root, cfg.stale_after_days, cfg.archive_after_days);
    if !archived.is_empty() {
        tracing::info!("learn: auto-archived {} stale skill(s): {}", archived.len(), archived.join(", "));
    }

    // 2. Gap detection over recent diaries (skip our own).
    let diaries = read_all_diaries(workspace_root, &settings.diary.root, 7);
    let mut lib = skills::read_skills(&operator_root, "personal");
    lib.extend(skills::read_skills(&repo_root, "repo"));
    let library = render_library(&lib);

    let mut gap_summary = "no diaries to scan".to_string();
    if !diaries.trim().is_empty() {
        let prompt = build_gap_prompt(&operator_root, &library, &diaries);
        let (reply, q) =
            run_skill_session(workspace_root, settings, &operator_root, "gap", &prompt).await?;
        gap_summary = truncate(&reply, 200);
        if !q.is_empty() {
            gap_summary = format!("{gap_summary} (quarantined: {})", q.join(", "));
        }
    }

    // 3. Curator consolidation over the (refreshed) agent-created library.
    let mut lib2 = skills::read_skills(&operator_root, "personal");
    lib2.extend(skills::read_skills(&repo_root, "repo"));
    let curate_summary = {
        let prompt = build_curate_prompt(&operator_root, &render_library(&lib2));
        let (reply, q) =
            run_skill_session(workspace_root, settings, &operator_root, "curate", &prompt).await?;
        let mut s = truncate(&reply, 200);
        if !q.is_empty() {
            s = format!("{s} (quarantined: {})", q.join(", "));
        }
        s
    };

    let summary = format!(
        "stale={stale} archived={} · gap: {gap_summary} · curate: {curate_summary}",
        archived.len()
    );
    let _ = diary::record_observation(workspace_root, AGENT_NAME, "learn", &summary, diary::Tag::Observation);
    tracing::info!("learn: {summary}");
    Ok(())
}

/// Archive agent-created, unpinned skills whose last activity is older than
/// `archive_days`; return (stale_count, archived_names). "Activity" = the
/// later of frontmatter `last_used` and the SKILL.md mtime. Hand-written
/// skills (created_by != "agent") and pinned skills are never auto-managed.
fn apply_auto_transitions(root: &Path, stale_days: u32, archive_days: u32) -> (usize, Vec<String>) {
    let mut stale = 0usize;
    let mut archived = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return (0, archived),
    };
    let now = chrono::Utc::now();
    for dirent in entries.flatten() {
        let dir = dirent.path();
        if !dir.is_dir() {
            continue;
        }
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        if name.starts_with('.') {
            continue;
        }
        let skill_md = dir.join(skills::SKILL_FILE);
        let Ok(content) = std::fs::read_to_string(&skill_md) else { continue };
        let fm = skills::parse_frontmatter(&content, &skill_md).unwrap_or_default();
        if fm.created_by.as_deref() != Some("agent") || fm.pinned {
            continue; // only auto-manage our own, never pinned
        }
        let age_days = skill_age_days(&fm, &skill_md, now);
        if age_days >= archive_days as i64 {
            let ts = now.format("%Y%m%dT%H%M%S");
            let dest = root.join(".archive").join(format!("{name}-{ts}"));
            if let Some(p) = dest.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            if std::fs::rename(&dir, &dest).is_ok() {
                archived.push(name);
            }
        } else if age_days >= stale_days as i64 {
            stale += 1;
        }
    }
    (stale, archived)
}

/// Days since a skill's last activity: max(last_used, mtime).
fn skill_age_days(fm: &skills::Frontmatter, skill_md: &Path, now: chrono::DateTime<chrono::Utc>) -> i64 {
    let mut newest: Option<chrono::DateTime<chrono::Utc>> = None;
    if let Some(lu) = &fm.last_used {
        // accept YYYY-MM-DD or RFC3339
        if let Ok(d) = chrono::NaiveDate::parse_from_str(lu.trim(), "%Y-%m-%d") {
            newest = d.and_hms_opt(0, 0, 0).map(|dt| dt.and_utc());
        } else if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(lu.trim()) {
            newest = Some(dt.with_timezone(&chrono::Utc));
        }
    }
    if let Ok(meta) = std::fs::metadata(skill_md) {
        if let Ok(modified) = meta.modified() {
            let dt: chrono::DateTime<chrono::Utc> = modified.into();
            newest = Some(newest.map_or(dt, |n| n.max(dt)));
        }
    }
    match newest {
        Some(n) => (now - n).num_days(),
        None => 0,
    }
}

/// Concatenate the last `days` of every agent's diary (skipping our own and
/// hidden files) so the gap pass sees what the system has been doing.
fn read_all_diaries(workspace_root: &Path, diary_root_rel: &str, days: i64) -> String {
    let root = workspace_root.join(diary_root_rel);
    let today = chrono::Local::now().date_naive();
    let mut out = String::new();
    let Ok(entries) = std::fs::read_dir(&root) else { return out };
    for dirent in entries.flatten() {
        let dir = dirent.path();
        if !dir.is_dir() {
            continue;
        }
        let agent = dir.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        if agent == AGENT_NAME || agent.starts_with('.') {
            continue; // never learn from our own diary
        }
        let mut agent_block = String::new();
        for back in 0..days {
            let Some(date) = today.checked_sub_signed(chrono::Duration::days(back)) else { continue };
            let path = dir.join(format!("{date}.md"));
            if let Ok(c) = std::fs::read_to_string(&path) {
                agent_block.push_str(&c);
                agent_block.push('\n');
            }
        }
        if !agent_block.trim().is_empty() {
            out.push_str(&format!("\n### agent: {agent}\n{agent_block}\n"));
        }
    }
    out
}

fn build_gap_prompt(operator_root: &Path, library: &str, diaries: &str) -> String {
    let dir = operator_root.display();
    format!(
        r#"You are Nucleus' skill-gap detector. Below are recent diary entries from every agent (what the system has actually been doing) and the current skill library. Find RECURRING tasks or workflows that lack a skill and would clearly benefit from one, and CREATE a class-level skill for each.

Rules:
  • Only create a skill for a pattern that recurs or is clearly a reusable class of work — not a one-off task someone did once.
  • Do NOT duplicate or near-duplicate an existing library skill. If the gap is "an existing skill is thin", that's the periodic curator's job, not yours — skip it.
  • Class-level names only (no dates, PR numbers, codenames).
  • Write to {dir}/<skill-name>/SKILL.md (operator-personal tree ONLY) via your file tools, with the required contract:
      ---
      name: <kebab-case>
      description: <one line>
      flavor: learned
      created_by: agent
      last_used: {today}
      ---
      # When to invoke
      …
      # Steps
      …
      # Failure modes
      …  (REQUIRED, never empty)
  • Never write a secret, token, email, phone, or personal identifier into a skill.

Current skill library:
{library}

Recent agent diaries:
---
{diaries}
---

Create the missing skills now, then reply with ONE line per skill created, or exactly "No gaps."."#,
        today = chrono::Local::now().format("%Y-%m-%d"),
    )
}

/// Ported Hermes CURATOR_REVIEW_PROMPT, adapted: candidates are the
/// agent-created skills; consolidation happens via file tools; archive =
/// move the dir into `.archive/` (never delete); pinned + hand-written
/// (created_by != agent) skills are off-limits.
#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, name: &str, frontmatter: &str, backdate: bool) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let md = dir.join("SKILL.md");
        std::fs::write(
            &md,
            format!("---\nname: {name}\ndescription: d\nflavor: learned\n{frontmatter}\n---\n\n# When to invoke\nx\n# Steps\n1\n# Failure modes\n- z\n"),
        )
        .unwrap();
        if backdate {
            // mtime is part of the activity anchor — backdate it well past
            // archive_after_days so the age test is meaningful.
            let _ = std::process::Command::new("touch")
                .args(["-t", "202001010000", md.to_str().unwrap()])
                .status();
        }
    }

    #[test]
    fn auto_transitions_archive_old_agent_skills_only() {
        let root = std::env::temp_dir().join(format!(
            "sgl-auto-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();

        // old + agent + unpinned → archived
        write_skill(&root, "old-agent", "created_by: agent\nlast_used: 2020-01-01", true);
        // old + agent + pinned → skipped
        write_skill(&root, "pinned-agent", "created_by: agent\npinned: true\nlast_used: 2020-01-01", true);
        // old + hand-written (no created_by) → skipped
        write_skill(&root, "hand-written", "last_used: 2020-01-01", true);
        // fresh agent → skipped (not old)
        write_skill(&root, "fresh-agent", "created_by: agent", false);

        let (_stale, archived) = apply_auto_transitions(&root, 30, 90);

        assert_eq!(archived, vec!["old-agent".to_string()], "only the old unpinned agent skill archives");
        assert!(root.join(".archive").exists(), "archive dir created");
        assert!(!root.join("old-agent").exists(), "archived skill moved out");
        assert!(root.join("pinned-agent").exists(), "pinned skill stays");
        assert!(root.join("hand-written").exists(), "hand-written skill stays");
        assert!(root.join("fresh-agent").exists(), "fresh skill stays");

        let _ = std::fs::remove_dir_all(&root);
    }
}

fn build_curate_prompt(operator_root: &Path, library: &str) -> String {
    let dir = operator_root.display();
    format!(
        r#"You are Nucleus' background skill CURATOR. This is an UMBRELLA-BUILDING consolidation pass, not a passive audit and not a duplicate-finder. The goal is a LIBRARY OF CLASS-LEVEL skills — one broad umbrella with labeled subsections beats five narrow siblings for discoverability (an agent matches skills on description, not exact name).

Hard rules — do not violate:
  1. Only touch skills with `created_by: agent` in their frontmatter. NEVER touch hand-written skills (no created_by, or created_by != agent) or anything marked `pinned: true`.
  2. NEVER delete a skill. The maximum action is ARCHIVING — move its directory into {dir}/.archive/ (recoverable). Use your terminal/file tools: `mv {dir}/<name> {dir}/.archive/<name>`.
  3. Do not use age/recency as a reason to skip consolidation — judge overlap on CONTENT.
  4. "Each has a distinct trigger" is NOT a reason to keep them separate. The bar is: would a maintainer write these as N skills, or one skill with N labeled subsections? If the latter, merge.

How to work:
  1. Identify clusters of agent-created skills sharing a domain/first word.
  2. For each cluster of 2+: pick or create the umbrella (a class-level SKILL.md), patch it to add a labeled subsection (or a references/<topic>.md support file) for each sibling's unique content, then ARCHIVE the absorbed siblings.
  3. Keep the SKILL.md contract intact on anything you write (frontmatter incl. flavor: learned + created_by: agent + the # When to invoke / # Steps / # Failure modes sections). Bump last_used: {today} on skills you patch.

Current skill library (only act on created_by: agent entries):
{library}

Do the consolidation now via your file tools, then reply with a short summary: which umbrellas you built and which siblings you archived into them. If nothing needs consolidating, reply exactly "Library is already well-shaped."."#,
        today = chrono::Local::now().format("%Y-%m-%d"),
    )
}
