//! skill-gap-learner — the all-facets successor to preference-learner (ADR-017).
//!
//! Two arms, one review engine, ported from Hermes' skill-review + curator:
//!   review  — on-the-fly: read ONE conversation transcript, autonomously
//!             create/patch skills it warrants. Fired detached by the
//!             conversational agents every N turns.
//!   learn   — periodic (launchd-cron): propose skills for recurring patterns
//!             across all diaries, then curate (stale/archive + consolidate).
//!             The diary window stretches back over missed runs (ADR-029
//!             watermark).
//!
//! All arms run in one claude session per local day (ADR-029 daily session,
//! key `skill-gap-learner`).
//!
//! Autonomous writes go to `<workspace>/.nucleus/.claude/skills/` only
//! (`skills::personal_skills_root` — operator-personal, gitignored, Rule 1);
//! never to the repo's committed `.claude/skills/`. Every touched SKILL.md is
//! run through `nucleus_core::skills::validate`; a malformed write is
//! quarantined to `.rejected/` (inside the personal root) instead of polluting
//! the live library.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use nucleus_core::{
    chore_state,
    claude_session::{last_n_turns, TurnRole},
    config::Settings,
    diary, skills,
    session_profile::{ProfileContext, SessionProfile},
};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const AGENT_NAME: &str = "skill-gap-learner";
const TMUX_SESSION: &str = "nucleus-skill-gap-learner";

/// Watermark key for the periodic arm: the local date of the last `learn`
/// that completed (ADR-029). The gap pass reads at least
/// [`GAP_WINDOW_DAYS`] of diary, and further back when runs were missed.
const LEARN_WATERMARK_KEY: &str = "skill-gap-learner.learn";
/// The minimum diary window the gap pass reasons over.
const GAP_WINDOW_DAYS: i64 = 7;

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

/// Entry point for this subcommand of the `nucleus` binary. `args` is the
/// full argv for the subcommand, argv[0] included, so clap renders usage
/// under the right name.
pub async fn run(args: Vec<std::ffi::OsString>) -> Result<()> {
    nucleus_core::init_tracing();
    let settings = Settings::load().context("loading settings")?;
    let workspace_root = std::env::current_dir()?;
    let cli = Cli::parse_from(args);

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

/// On-the-fly review of a single conversation (the Hermes skill-review arm).
async fn review(
    workspace_root: &Path,
    settings: &Settings,
    transcript: &Path,
    venue: &str,
    chat_key: Option<&str>,
) -> Result<()> {
    let operator_root = skills::personal_skills_root(workspace_root);
    let repo_root = skills::repo_skills_root(workspace_root);

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
    let library = library_summary(&operator_root, &repo_root, machine_skills_root().as_deref());

    let prompt = build_review_prompt(&operator_root, &library, venue, &conversation);
    let (reply, quarantined) =
        run_skill_session(workspace_root, settings, &format!("review-{venue}"), &prompt).await?;

    let summary = if quarantined.is_empty() {
        format!("reviewed {venue} ({} turns): {}", turns.len(), truncate(&reply, 240))
    } else {
        format!(
            "reviewed {venue}: gate quarantined [{}] flagged [{}] committed-tree writes [{}]",
            quarantined.quarantined.join(", "),
            quarantined.flagged.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", "),
            quarantined.repo_writes.join(", ")
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

#[derive(Default)]
struct GateOutcome {
    /// Agent-authored skills moved to `.rejected/`.
    quarantined: Vec<String>,
    /// Personal skills left in place with a problem the operator should
    /// see: a hand-written skill that fails the contract, or a skill whose
    /// name collides with a machine-wide or committed skill. (name, reason)
    flagged: Vec<(String, String)>,
    /// Files under the committed `.claude/skills/` tree modified during the
    /// run, relative to that tree. The learner must never write there; these
    /// are reported only — the committed tree is hand-owned, so nothing is
    /// moved or deleted.
    repo_writes: Vec<String>,
}

impl GateOutcome {
    fn is_empty(&self) -> bool {
        self.quarantined.is_empty() && self.flagged.is_empty() && self.repo_writes.is_empty()
    }
    fn names(&self) -> Vec<String> {
        let mut v = self.quarantined.clone();
        v.extend(self.flagged.iter().map(|(n, _)| n.clone()));
        v.extend(self.repo_writes.iter().map(|p| format!(".claude/skills/{p}")));
        v
    }
}

/// `$HOME/.claude/skills` — the machine-wide skills tree Claude Code loads in
/// every project. A skill there wins over a same-named project or `--add-dir`
/// skill. `None` when HOME is unset.
fn machine_skills_root() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".claude").join("skills"))
}

/// Skill names already taken outside the personal tree, keyed to the reason
/// a personal skill with that name is a problem. Both the directory name and
/// the frontmatter `name` count. Machine-wide entries take precedence over
/// repo entries because the machine-wide copy is the one that loads.
fn reserved_skill_names(
    repo_root: &Path,
    machine_root: Option<&Path>,
) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let mut add = |root: &Path, tier: &str, reason: &dyn Fn(&str) -> String| {
        for s in skills::read_skills(root, tier) {
            let dir_name = Path::new(&s.path)
                .parent()
                .and_then(|d| d.file_name())
                .and_then(|n| n.to_str())
                .map(str::to_string);
            for key in std::iter::once(s.name.clone()).chain(dir_name) {
                out.entry(key).or_insert_with(|| reason(&s.name));
            }
        }
    };
    if let Some(machine) = machine_root {
        add(machine, "machine", &|n| {
            format!("the machine-wide skill `{n}` in ~/.claude/skills has the same name and shadows it — this copy never loads")
        });
    }
    add(repo_root, "repo", &|n| {
        format!("duplicates the committed skill `{n}` in .claude/skills")
    });
    out
}

/// Every file under `repo_root` modified at or after `since`, as paths
/// relative to `repo_root`, sorted. Missing root → empty.
fn repo_files_modified_since(repo_root: &Path, since: SystemTime) -> Vec<String> {
    fn walk(dir: &Path, base: &Path, since: SystemTime, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for dirent in entries.flatten() {
            let path = dirent.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
            if meta.is_dir() {
                walk(&path, base, since, out);
            } else if meta.modified().is_ok_and(|m| m >= since) {
                if let Ok(rel) = path.strip_prefix(base) {
                    out.push(rel.to_string_lossy().into_owned());
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(repo_root, repo_root, since, &mut out);
    out.sort();
    out
}

/// Validate every SKILL.md under `root` (the personal tree) modified since
/// `since`; move any that fail to `<root>/.rejected/<name>-<ts>/`. Returns
/// what the gate did.
///
/// Also, for the same run:
///   - flags a touched personal skill whose directory name or frontmatter
///     `name` matches a skill in `machine_root` (which shadows it) or in
///     `repo_root` (a duplicate of a committed skill);
///   - reports every file under `repo_root` modified since `since`. The
///     session's cwd is the workspace root, so a wrong relative path lands
///     in the committed tree. Those files are hand-owned and are only
///     reported, never moved.
///
/// Only skills this agent authored (`created_by: agent`) are ever moved.
/// A hand-written skill is reported and left alone: the format contract
/// exists to police autonomous writes, and an operator skill never agreed
/// to it. On 2026-08-28 the curator made a cosmetic frontmatter fix to a
/// hand-written skill, that touch made it eligible for the gate, and a missing
/// `# Steps` heading moved the whole directory out of the library — taking
/// the script a reminder depended on with it.
fn gate_touched_skills(
    root: &Path,
    repo_root: &Path,
    machine_root: Option<&Path>,
    since: SystemTime,
) -> Result<GateOutcome> {
    let mut outcome = GateOutcome {
        repo_writes: repo_files_modified_since(repo_root, since),
        ..GateOutcome::default()
    };
    for rel in &outcome.repo_writes {
        tracing::warn!("review: session wrote to the committed tree: .claude/skills/{rel}");
    }
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return Ok(outcome),
    };
    let reserved = reserved_skill_names(repo_root, machine_root);
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
        let fm = skills::parse_frontmatter(&content, &skill_md).unwrap_or_default();
        let issues = skills::validate(&content);
        if issues.is_empty() || fm.created_by.as_deref() != Some("agent") || fm.pinned {
            // Stays in the live tree: report the problems it has there.
            if !issues.is_empty() {
                // Same ownership rule as apply_auto_transitions: only
                // auto-manage what this agent wrote, and never touch a
                // pinned skill.
                tracing::warn!("review: `{name}` fails the contract but is hand-written — left in place ({issues:?})");
                outcome
                    .flagged
                    .push((name.clone(), format!("fails the SKILL.md contract ({})", issues.join("; "))));
            }
            let collision = reserved
                .get(&name)
                .or_else(|| fm.name.as_deref().and_then(|n| reserved.get(n)));
            if let Some(reason) = collision {
                tracing::warn!("review: `{name}` name collision — {reason}");
                outcome.flagged.push((name, reason.clone()));
            }
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
                outcome.quarantined.push(name);
            }
            Err(e) => tracing::warn!("review: failed to quarantine `{name}`: {e}"),
        }
    }
    Ok(outcome)
}

/// Tell the operator that the gate touched his library.
///
/// A quarantine removes a skill from the live tree, and anything depending
/// on that directory breaks with it — a reminder's condition watcher, a
/// script another skill shells out to. Before 2026-08-28 this was recorded
/// only in the diary and the log, so the first sign was a reminder that
/// stopped arriving. Delivery goes through the `reminders` binary so it
/// reuses the channel fan-out and the per-channel retry.
async fn alert_gate(outcome: &GateOutcome) {
    if outcome.is_empty() {
        return;
    }
    let mut lines = Vec::new();
    for name in &outcome.quarantined {
        lines.push(format!(
            "• `{name}` moved to .rejected/ — anything pointing at that directory is now broken"
        ));
    }
    for (name, reason) in &outcome.flagged {
        lines.push(format!("• `{name}` {reason} — left in place"));
    }
    for rel in &outcome.repo_writes {
        lines.push(format!(
            "• `.claude/skills/{rel}` was written in the committed tree — the learner writes only to the personal tree; review it with git status (left in place)"
        ));
    }
    let body = format!("⚠️ skill-gap-learner touched the skill library\n\n{}", lines.join("\n"));

    // Both halves live in the same binary since ADR-030, so this is a library
    // call rather than a subprocess. It also removes the old silent failure
    // mode: the alert used to be dropped whenever target/release/reminders
    // happened not to exist.
    let at = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%:z").to_string();
    let out = reminders::run(
        ["nucleus reminders", "add", "--at", &at, "--title", "skill gate",
         "--body", &body, "--channels", "discord-home,whatsapp-dm"]
            .iter()
            .map(std::ffi::OsString::from)
            .collect(),
    )
    .await;
    match out {
        Ok(()) => {
            tracing::info!("gate alert queued for {}", outcome.names().join(", "))
        }
        Err(e) => tracing::warn!("gate alert failed: {e:#}"),
    }
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

/// The library text every prompt shows: the personal + repo skills, then the
/// machine-wide skills as a separate read-only section. A machine-wide skill
/// loads in every project and wins over a same-named skill here, so the
/// model must neither recreate nor edit one. The section is omitted when
/// HOME or the machine-wide tree is missing or empty.
fn library_summary(operator_root: &Path, repo_root: &Path, machine_root: Option<&Path>) -> String {
    let mut lib = skills::read_skills(operator_root, "personal");
    lib.extend(skills::read_skills(repo_root, "repo"));
    let mut out = render_library(&lib);
    let machine = machine_root.map(|r| skills::read_skills(r, "machine")).unwrap_or_default();
    if !machine.is_empty() {
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(
            "\nMachine-wide skills (~/.claude/skills — READ-ONLY: they load in every session and shadow a same-named skill; do not recreate, patch, archive, or reuse these names):\n",
        );
        out.push_str(&render_library(&machine));
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
  • Write skills with your own Read/Write/Edit file tools, under {dir}/<skill-name>/SKILL.md (the gitignored operator-personal tree ONLY — never the repo's committed .claude/skills at the workspace root).
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
    window: &str,
    prompt: &str,
) -> Result<(String, GateOutcome)> {
    let operator_root = skills::personal_skills_root(workspace_root);
    let operator_root = operator_root.as_path();
    // Create the personal root up front: the session writes into it, and its
    // `.nucleus` ancestor must exist for `build_claude_args` to pass it as
    // `--add-dir` (which is what makes the written skills load).
    std::fs::create_dir_all(operator_root)
        .with_context(|| format!("creating {}", operator_root.display()))?;
    let started = SystemTime::now();
    // ADR-029: every arm — each on-the-fly review, the gap pass, the
    // curator — resumes one session per local day.
    let outcome = SessionProfile::one_shot_agentic(&ProfileContext {
        workspace_root,
        claude: &settings.claude,
        tmux_session: TMUX_SESSION,
        agent_label: AGENT_NAME,
    })
    .window_name(window)
    .daily_session(AGENT_NAME)
    .run_one_shot(prompt)
    .await
    .with_context(|| format!("skill session ({window})"))?;
    let gate = gate_touched_skills(
        operator_root,
        &skills::repo_skills_root(workspace_root),
        machine_skills_root().as_deref(),
        started,
    )?;
    alert_gate(&gate).await;
    Ok((outcome.reply, gate))
}

// ── periodic arm: gap detection + curator ──────────────────────────────────

/// The periodic pass (launchd-cron). Pure auto-archive of stale agent-created
/// skills, then an LLM gap-detection pass over recent diaries, then an LLM
/// curator/consolidation pass. Mirrors the distiller's two-phase shape.
async fn learn(workspace_root: &Path, settings: &Settings) -> Result<()> {
    let operator_root = skills::personal_skills_root(workspace_root);
    let repo_root = skills::repo_skills_root(workspace_root);
    let cfg = &settings.skill_learner;

    // 1. Pure auto-transitions (no LLM): archive agent-created, unpinned skills
    // idle past archive_after_days; count the merely-stale for the log.
    let (stale, archived) =
        apply_auto_transitions(&operator_root, cfg.stale_after_days, cfg.archive_after_days);
    if !archived.is_empty() {
        tracing::info!("learn: auto-archived {} stale skill(s): {}", archived.len(), archived.join(", "));
    }

    // 2. Gap detection over recent diaries (skip our own). The window is
    // GAP_WINDOW_DAYS, stretched back to the day after the last completed
    // run when nights were missed (ADR-029 watermark).
    let today = chrono::Local::now().date_naive();
    let from = chore_state::resume_date_after(workspace_root, LEARN_WATERMARK_KEY, GAP_WINDOW_DAYS - 1)
        .await?
        .min(today - chrono::Duration::days(GAP_WINDOW_DAYS - 1));
    let window_days = (today - from).num_days() + 1;
    if window_days > GAP_WINDOW_DAYS {
        tracing::info!("learn: catching up — gap window is {window_days} days (from {from})");
    }
    let diaries = read_all_diaries(workspace_root, &settings.diary.root, window_days);
    let library = library_summary(&operator_root, &repo_root, machine_skills_root().as_deref());

    let mut gap_summary = "no diaries to scan".to_string();
    if !diaries.trim().is_empty() {
        let prompt = build_gap_prompt(&operator_root, &library, &diaries);
        let (reply, q) =
            run_skill_session(workspace_root, settings, "gap", &prompt).await?;
        gap_summary = truncate(&reply, 200);
        if !q.is_empty() {
            gap_summary = format!("{gap_summary} (gate: {})", q.names().join(", "));
        }
    }

    // 3. Curator consolidation over the (refreshed) agent-created library.
    let library = library_summary(&operator_root, &repo_root, machine_skills_root().as_deref());
    let curate_summary = {
        let prompt = build_curate_prompt(&operator_root, &library);
        let (reply, q) =
            run_skill_session(workspace_root, settings, "curate", &prompt).await?;
        let mut s = truncate(&reply, 200);
        if !q.is_empty() {
            s = format!("{s} (gate: {})", q.names().join(", "));
        }
        s
    };

    let summary = format!(
        "stale={stale} archived={} · gap: {gap_summary} · curate: {curate_summary}",
        archived.len()
    );
    let nothing_happened = archived.is_empty()
        && gap_summary.starts_with("No gaps")
        && curate_summary.starts_with("Library is already well-shaped");
    let _ = diary::record_observation(
        workspace_root,
        AGENT_NAME,
        "learn",
        &summary,
        if nothing_happened { diary::Tag::Routine } else { diary::Tag::Observation },
    );
    tracing::info!("learn: {summary}");
    // Today's diaries are still being written; mark yesterday so the next
    // run re-reads today in full (same rule as the distiller).
    if let Some(yesterday) = today.pred_opt() {
        chore_state::set_watermark(workspace_root, LEARN_WATERMARK_KEY, &yesterday.to_string()).await?;
    }
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
            continue;
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
                let kept = diary::without_routine(&c);
                if !kept.trim().is_empty() {
                    agent_block.push_str(&format!("#### {date}\n\n{kept}"));
                }
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
  • Write to {dir}/<skill-name>/SKILL.md (the gitignored operator-personal tree ONLY — never the repo's committed .claude/skills at the workspace root) via your file tools, with the required contract:
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
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();

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
    }

    /// Workspace with the personal + repo trees and a separate machine-wide
    /// tree, each holding one pre-existing (backdated) skill.
    struct Trees {
        _tmp: tempfile::TempDir,
        personal: PathBuf,
        repo: PathBuf,
        machine: PathBuf,
    }

    fn trees() -> Trees {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let personal = skills::personal_skills_root(&ws);
        let repo = skills::repo_skills_root(&ws);
        let machine = tmp.path().join("home/.claude/skills");
        write_skill(&repo, "committed-one", "", true);
        write_skill(&machine, "machine-one", "", true);
        write_skill(&personal, "old-personal", "created_by: agent", true);
        Trees { _tmp: tmp, personal, repo, machine }
    }

    /// Backdated fixtures are from 2020, so one second of slack cannot let
    /// them count as touched, while it absorbs coarse mtime resolution.
    fn run_start() -> SystemTime {
        SystemTime::now() - std::time::Duration::from_secs(1)
    }

    #[test]
    fn gate_reports_committed_tree_writes_without_touching_them() {
        let t = trees();
        let since = run_start();
        write_skill(&t.repo, "stray-write", "created_by: agent", false);
        std::fs::create_dir_all(t.repo.join("committed-one/scripts")).unwrap();
        std::fs::write(t.repo.join("committed-one/scripts/run.sh"), "true\n").unwrap();

        let out = gate_touched_skills(&t.personal, &t.repo, Some(&t.machine), since).unwrap();

        assert_eq!(
            out.repo_writes,
            vec!["committed-one/scripts/run.sh".to_string(), "stray-write/SKILL.md".to_string()]
        );
        assert!(out.quarantined.is_empty() && out.flagged.is_empty());
        assert!(!out.is_empty(), "a committed-tree write must trigger the alert");
        assert!(t.repo.join("stray-write/SKILL.md").exists(), "repo skills are never moved");
        assert!(t.repo.join("committed-one/SKILL.md").exists());
    }

    #[test]
    fn gate_flags_name_collisions_with_machine_and_repo_skills() {
        let t = trees();
        let since = run_start();
        // Same directory name as a machine-wide skill.
        write_skill(&t.personal, "machine-one", "created_by: agent", false);
        // Different directory, frontmatter `name` of a committed skill.
        let dir = t.personal.join("renamed-dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            "---\nname: committed-one\ndescription: d\nflavor: learned\ncreated_by: agent\n---\n\n# When to invoke\nx\n# Steps\n1\n# Failure modes\n- z\n",
        )
        .unwrap();
        // Touched, unique name: no flag.
        write_skill(&t.personal, "unique-new", "created_by: agent", false);
        // Colliding name but not touched this run: no flag.
        write_skill(&t.personal, "committed-one", "created_by: agent", true);

        let out = gate_touched_skills(&t.personal, &t.repo, Some(&t.machine), since).unwrap();

        let mut flagged = out.flagged.clone();
        flagged.sort();
        assert_eq!(flagged.len(), 2, "{flagged:?}");
        assert_eq!(flagged[0].0, "machine-one");
        assert!(flagged[0].1.contains("machine-wide") && flagged[0].1.contains("shadows"));
        assert_eq!(flagged[1].0, "renamed-dir");
        assert!(flagged[1].1.contains("duplicates the committed skill `committed-one`"));
        assert!(out.quarantined.is_empty() && out.repo_writes.is_empty());
        assert!(t.personal.join("machine-one").exists(), "a collision is only flagged");

        // Without a machine-wide tree (HOME unset) only the repo collision remains.
        let out = gate_touched_skills(&t.personal, &t.repo, None, since).unwrap();
        assert_eq!(out.flagged.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(), vec!["renamed-dir"]);
    }

    #[test]
    fn library_summary_lists_machine_skills_as_a_read_only_section() {
        let t = trees();
        let with = library_summary(&t.personal, &t.repo, Some(&t.machine));
        let (main, machine) = with.split_once("Machine-wide skills").expect("machine section present");
        assert!(main.contains("- old-personal [personal]: d"));
        assert!(main.contains("- committed-one [repo]: d"));
        assert!(!main.contains("machine-one"));
        assert!(machine.contains("READ-ONLY"));
        assert!(machine.contains("- machine-one [machine]: d"));

        let without = library_summary(&t.personal, &t.repo, None);
        assert!(!without.contains("Machine-wide"));
        let missing = t.machine.join("does-not-exist");
        assert_eq!(library_summary(&t.personal, &t.repo, Some(&missing)), without);
    }

    #[test]
    fn prompts_target_the_personal_root_only() {
        let root = skills::personal_skills_root(Path::new("/ws"));
        let dir = root.display().to_string();
        assert_eq!(dir, "/ws/.nucleus/.claude/skills");
        let prompts = [
            build_review_prompt(&root, "lib", "discord", "conv"),
            build_gap_prompt(&root, "lib", "diaries"),
            build_curate_prompt(&root, "lib"),
        ];
        for p in &prompts {
            assert!(p.contains(&format!("{dir}/")), "prompt must name the personal root");
            assert!(p.contains("never the repo's committed .claude/skills") || p.contains("NEVER write to or move anything in the repo's committed .claude/skills"));
            assert!(!p.contains("~/.claude/skills"), "no home-tree wording left");
        }
        assert!(prompts[2].contains(&format!("{dir}/.archive/")), "curator archives inside the personal root");
    }
}

fn build_curate_prompt(operator_root: &Path, library: &str) -> String {
    let dir = operator_root.display();
    format!(
        r#"You are Nucleus' background skill CURATOR. This is an UMBRELLA-BUILDING consolidation pass, not a passive audit and not a duplicate-finder. The goal is a LIBRARY OF CLASS-LEVEL skills — one broad umbrella with labeled subsections beats five narrow siblings for discoverability (an agent matches skills on description, not exact name).

Hard rules — do not violate:
  1. Only work inside {dir} (the gitignored operator-personal tree). NEVER write to or move anything in the repo's committed .claude/skills at the workspace root.
  2. Only touch skills with `created_by: agent` in their frontmatter. NEVER touch hand-written skills (no created_by, or created_by != agent) or anything marked `pinned: true`.
  3. NEVER delete a skill. The maximum action is ARCHIVING — move its directory into {dir}/.archive/ (recoverable). Use your terminal/file tools: `mv {dir}/<name> {dir}/.archive/<name>`.
  4. Do not use age/recency as a reason to skip consolidation — judge overlap on CONTENT.
  5. "Each has a distinct trigger" is NOT a reason to keep them separate. The bar is: would a maintainer write these as N skills, or one skill with N labeled subsections? If the latter, merge.

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
