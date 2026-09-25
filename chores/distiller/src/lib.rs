//! distiller — diary distillation, one consolidated daily pass (ADR-016).
//!
//! No subcommand. Each invocation:
//!   1. metabolism    — extract candidates from each agent's diary since its
//!                      watermark, a date and byte offset, so no diary byte
//!                      is read twice (normally the rest of yesterday +
//!                      today; a failed night or a machine that was off is
//!                      caught up in 2-day windows, ADR-029) → _pending.md
//!   2. contemplation — judge them (PROMOTE | MERGE | ARCHIVE | DROP) + prune
//!
//! Both passes run in one claude session per local day (ADR-029 daily
//! session, key `distiller`).
//!
//! Persona auto-evolution (ADR-004's "SOUL slot") is intentionally NOT here —
//! that's deferred to the future skill-gap learner (ADR-016), which proposes
//! persona edits as reviewable suggestions rather than silent writes to the
//! operator-personal `personas/<slug>.md` files.

use anyhow::{Context, Result};
use chrono::{Duration, Local, NaiveDate};
use nucleus_core::{
    chore_state,
    config::Settings,
    diary, memory,
    session_profile::{ProfileContext, SessionProfile},
};
use serde::Deserialize;
use std::path::{Path, PathBuf};

const AGENT_NAME: &str = "distiller";
/// ADR-035 vault search, run by the contemplation session from the
/// workspace root to find a note to append to.
const VAULT_SEARCH_CMD: &str = "./target/release/nucleus vault-search";

/// Days of diary one metabolism ask covers: yesterday plus today, the shape
/// the daily pass has always pasted. A catch-up after failed nights walks the
/// missed range in windows of this size so no single ask grows with the
/// outage.
const METABOLISM_WINDOW_DAYS: i64 = 2;

/// Per-agent metabolism watermark key. The value is a [`DiaryMark`].
fn metabolism_watermark_key(agent: &str) -> String {
    format!("distiller.metabolism.{agent}")
}

/// Per-agent contemplation progress key. The value is a [`PendingMark`].
fn contemplation_progress_key(agent: &str) -> String {
    format!("distiller.contemplation.{agent}")
}

/// How far metabolism has read an agent's diary: every day before `date`,
/// and the first `offset` bytes of `date`'s file.
///
/// Today's diary is still being written. A run reads it up to its last
/// complete line and records that byte offset; the next run (later the same
/// day, or the next day) starts at the offset, so each diary byte is sent to
/// the model once. Diary files only grow (entries are appended whole), so
/// the offset stays valid. Stored as `<date>@<offset>`. A bare `<date>` (the
/// earlier format) means that day was fully processed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DiaryMark {
    date: NaiveDate,
    offset: usize,
}

impl DiaryMark {
    fn parse(v: &str) -> Option<DiaryMark> {
        match v.split_once('@') {
            Some((d, o)) => Some(DiaryMark { date: d.parse().ok()?, offset: o.parse().ok()? }),
            None => Some(DiaryMark { date: v.parse::<NaiveDate>().ok()?.succ_opt()?, offset: 0 }),
        }
    }

    fn encode(&self) -> String {
        format!("{}@{}", self.date, self.offset)
    }
}

/// Where metabolism resumes for `key`: the stored mark, or yesterday when
/// there is none (the pre-watermark window). Never after today.
async fn metabolism_start(workspace_root: &Path, key: &str, today: NaiveDate) -> Result<DiaryMark> {
    let yesterday = DiaryMark { date: today.pred_opt().unwrap_or(today), offset: 0 };
    let mark = match chore_state::watermark(workspace_root, key).await? {
        Some(v) => DiaryMark::parse(&v).unwrap_or_else(|| {
            tracing::warn!(key, value = %v, "metabolism watermark is unreadable — starting at yesterday");
            yesterday
        }),
        None => yesterday,
    };
    Ok(if mark.date > today { DiaryMark { date: today, offset: 0 } } else { mark })
}

/// The diary from `start` through the end of `to` (routine entries removed,
/// see `diary::without_routine`), and the mark just after what was read.
/// Days before `today` are read whole; `today` is read up to its last
/// complete line, because the file is still being written.
fn read_window(agent_dir: &Path, start: DiaryMark, to: NaiveDate, today: NaiveDate) -> Result<(String, DiaryMark)> {
    let mut out = String::new();
    let mut date = start.date;
    let mut end = DiaryMark { date: to.succ_opt().unwrap_or(to), offset: 0 };
    while date <= to {
        let path = agent_dir.join(format!("{}.md", date));
        let text = if path.exists() {
            std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?
        } else {
            String::new()
        };
        let upto = if date == today { text.rfind('\n').map(|i| i + 1).unwrap_or(0) } else { text.len() };
        let skip = if date == start.date { start.offset } else { 0 };
        // A file shorter than the mark was replaced; read it from the start.
        let skip = if skip > upto { 0 } else { skip };
        let slice = text.get(skip..upto).unwrap_or("");
        let kept = diary::without_routine(slice);
        if !kept.trim().is_empty() {
            out.push_str(&format!("### {date}\n\n{kept}"));
        }
        if date == today {
            end = DiaryMark { date, offset: upto };
        }
        let Some(next) = date.succ_opt() else { break; };
        date = next;
    }
    Ok((out, end))
}

/// The model calls a distiller pass makes, separated so the passes can be
/// tested without a claude session.
trait Model {
    async fn ask(&mut self, prompt: &str) -> Result<String>;
}

struct SessionModel<'a> {
    session: &'a mut nucleus_core::claude_session::Session,
    opts: &'a nucleus_core::claude_session::AskOptions,
}

impl Model for SessionModel<'_> {
    async fn ask(&mut self, prompt: &str) -> Result<String> {
        self.session.ask(prompt, self.opts.clone()).await
    }
}

/// Entry point for this subcommand of the `nucleus` binary. `args` is the
/// full argv for the subcommand, argv[0] included, so clap renders usage
/// under the right name.
pub async fn run(_args: Vec<std::ffi::OsString>) -> Result<()> {
    nucleus_core::init_tracing();
    let settings = Settings::load().context("loading settings")?;
    let workspace_root = settings.workspace_root()?;
    let diary_root = workspace_root.join(&settings.diary.root);

    let _ = tokio::process::Command::new("tmux")
        .args(["kill-session", "-t", "nucleus-distiller"])
        .output()
        .await;

    metabolism(&workspace_root, &diary_root, &settings).await?;
    contemplation(&workspace_root, &diary_root, &settings).await?;
    session_index_maintenance(&workspace_root, &settings).await;
    usage_maintenance(&workspace_root, &settings).await;
    Ok(())
}

/// ADR-034 daily usage refresh. Claude Code deletes transcripts after about
/// 30 days; a daily pass keeps `memory/usage.db` complete whether or not
/// anyone opens the dashboard. Best-effort like the index maintenance; a
/// refresh already running (started from the dashboard) is not an error.
async fn usage_maintenance(workspace_root: &Path, settings: &Settings) {
    use nucleus_core::usage;
    match usage::refresh(workspace_root, &settings.usage, usage::RefreshOptions::default()).await {
        Ok(s) => {
            let _ = nucleus_core::diary::record_observation(
                workspace_root,
                "distiller",
                "usage",
                &format!(
                    "usage refresh: {} of {} transcript files changed ({:.1} MB), {} records, {} sessions labeled, {:.1}s{}",
                    s.files_read,
                    s.files_seen,
                    s.bytes_read as f64 / 1e6,
                    s.records,
                    s.sessions_labeled,
                    s.elapsed.as_secs_f64(),
                    if s.partial() {
                        format!(
                            " — PARTIAL: {} file(s) not read, {} malformed and {} oversized line(s) skipped",
                            s.files_failed, s.malformed_lines, s.oversized_lines
                        )
                    } else {
                        String::new()
                    },
                ),
                nucleus_core::diary::Tag::Routine,
            );
        }
        Err(e) => {
            tracing::warn!(err = %format!("{e:#}"), "usage refresh failed");
        }
    }
}

/// ADR-023 daily catch-up: refresh the session-search index and prune
/// junk transcripts. Best-effort — index maintenance must never fail the
/// distillation pass. Prune stays dry-run until
/// `[session_search] prune_apply = true` in nucleus.toml; counts go to
/// the diary either way (no silent caps, ADR-020).
async fn session_index_maintenance(workspace_root: &Path, settings: &Settings) {
    use nucleus_core::session_index;
    let result = async {
        let pool = session_index::open(workspace_root).await?;
        let idx = session_index::update_index(&pool, workspace_root).await?;
        let prune = session_index::prune_junk(
            &pool,
            workspace_root,
            settings.session_search.prune_apply,
            settings.session_search.prune_max_age_days,
        )
        .await?;
        anyhow::Ok((idx, prune))
    }
    .await;
    match result {
        Ok((idx, prune)) => {
            let _ = nucleus_core::diary::record_observation(
                workspace_root,
                "distiller",
                "session-index",
                &format!(
                    "session-search index: {} (re)indexed, {} ineligible, {} unchanged; prune{}: {} junk candidate(s), {} deleted",
                    idx.indexed,
                    idx.ineligible,
                    idx.skipped_unchanged,
                    if prune.dry_run { " (dry-run)" } else { "" },
                    prune.candidates,
                    prune.deleted,
                ),
                nucleus_core::diary::Tag::Routine,
            );
        }
        Err(e) => {
            tracing::warn!(err = %format!("{e:#}"), "session-index maintenance failed");
        }
    }
}

fn list_agent_dirs(diary_root: &Path) -> Result<Vec<(String, PathBuf)>> {
    if !diary_root.exists() {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    for e in std::fs::read_dir(diary_root)? {
        let e = e?;
        if e.file_type()?.is_dir() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') { continue; }
            out.push((name, e.path()));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn read_recent_entries(agent_dir: &Path, since: chrono::DateTime<Local>) -> Result<String> {
    // Concatenate all daily diary files from `since` through today, inclusive.
    read_entries_between(agent_dir, since.date_naive(), Local::now().date_naive())
}

/// Concatenate the daily diary files from `from` through `to`, inclusive.
/// Missing days are skipped.
/// Concatenate the daily diary files from `from` through `to`, inclusive,
/// with routine entries removed (see `diary::without_routine`). Missing
/// days are skipped; a day of only routine entries contributes nothing.
fn read_entries_between(agent_dir: &Path, from: NaiveDate, to: NaiveDate) -> Result<String> {
    let mut out = String::new();
    let mut date = from;
    while date <= to {
        let path = agent_dir.join(format!("{}.md", date));
        if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let kept = diary::without_routine(&text);
            if !kept.trim().is_empty() {
                out.push_str(&format!("### {date}\n\n{kept}"));
            }
        }
        let Some(next) = date.succ_opt() else { break; };
        date = next;
    }
    Ok(out)
}

#[derive(Debug, Deserialize)]
struct Candidate {
    tag: String,
    body: String,
    #[serde(default)]
    confidence: f64,
}

async fn metabolism(workspace_root: &Path, diary_root: &Path, settings: &Settings) -> Result<()> {
    let agents = list_agent_dirs(diary_root)?;
    if agents.is_empty() {
        tracing::info!("metabolism: no agent diaries found");
        return Ok(());
    }
    // One session reused across agents — pays the ~5s spawn cost once
    // instead of per-agent. Daily-session continuity (ADR-029) makes it the
    // same session contemplation resumes, so a day's distillation is one
    // transcript. The profile supplies the posture (ADR-020 — this path used
    // to run without the Settings disallowed_tools).
    let (mut session, ask_opts) = SessionProfile::one_shot_utility(&ProfileContext {
        workspace_root,
        claude: &settings.claude,
        tmux_session: "nucleus-distiller",
        agent_label: "distiller",
    })
    .window_name("metabolism")
    .daily_session(AGENT_NAME)
    .spawn()
    .await
    .context("spawning claude session for metabolism")?;

    let today = Local::now().date_naive();
    let mut total_staged = 0usize;
    let mut agents_processed = 0usize;
    let mut model = SessionModel { session: &mut session, opts: &ask_opts };

    for (agent, agent_dir) in agents {
        if agent == AGENT_NAME { continue; }  // distiller doesn't extract from itself
        let run = metabolize_agent(&mut model, workspace_root, &agent, &agent_dir, today).await?;
        if run.parse_failed {
            tracing::warn!(
                "metabolism: agent {} — stopped at an unparsable reply; watermark held, will retry next run",
                agent
            );
        }
        if run.windows == 0 {
            continue;
        }
        agents_processed += 1;
        total_staged += run.staged;
        if run.windows > 1 {
            tracing::info!(
                "metabolism: agent {} — caught up {} windows from {}",
                agent, run.windows, run.from
            );
        }
        if run.staged == 0 {
            tracing::info!("metabolism: agent {} — no candidates", agent);
        } else {
            tracing::info!("metabolism: agent {} — {} candidates staged", agent, run.staged);
        }
    }

    let _ = session.close().await;

    let _ = diary::record_observation(
        workspace_root,
        AGENT_NAME,
        "metabolism",
        &format!("{} agents scanned, {} candidates staged", agents_processed, total_staged),
        if total_staged == 0 { diary::Tag::Routine } else { diary::Tag::Observation },
    );
    Ok(())
}

/// What one agent's metabolism did.
struct AgentRun {
    from: NaiveDate,
    windows: usize,
    staged: usize,
    parse_failed: bool,
}

/// Metabolism for one agent: from its watermark (ADR-029) through today in
/// windows of [`METABOLISM_WINDOW_DAYS`], one ask per window, advancing the
/// watermark after each window to exactly what that window read. A night the
/// pass failed, or a machine that was off, is caught up here instead of
/// skipped; content an earlier run already read is not read again.
async fn metabolize_agent(
    model: &mut impl Model,
    workspace_root: &Path,
    agent: &str,
    agent_dir: &Path,
    today: NaiveDate,
) -> Result<AgentRun> {
    let key = metabolism_watermark_key(agent);
    let mut start = metabolism_start(workspace_root, &key, today).await?;
    let mut run = AgentRun { from: start.date, windows: 0, staged: 0, parse_failed: false };
    while start.date <= today {
        let window_end = (start.date + Duration::days(METABOLISM_WINDOW_DAYS - 1)).min(today);
        let (body, next) = read_window(agent_dir, start, window_end, today)?;
        if !body.trim().is_empty() {
            run.windows += 1;
            // Ok(None): the reply did not parse. Stop this agent here so
            // its watermark stays on the last window whose candidates were
            // actually staged; the next run retries from there. An ask
            // error propagates — that is the session, not the agent.
            let Some(staged) = metabolize_window(model, agent, agent_dir, &body).await? else {
                run.parse_failed = true;
                break;
            };
            run.staged += staged;
        }
        if next != start {
            chore_state::set_watermark(workspace_root, &key, &next.encode()).await?;
        }
        if window_end >= today {
            break;
        }
        start = next;
    }
    Ok(run)
}

/// One metabolism ask: extract candidates from `body` (one window of an
/// agent's diary) and stage them in `_pending.md`. `Ok(Some(n))` = n
/// candidates staged; `Ok(None)` = the reply did not parse, nothing staged
/// and the caller must not advance past this window; `Err` = the ask failed.
async fn metabolize_window(
    model: &mut impl Model,
    agent: &str,
    agent_dir: &Path,
    body: &str,
) -> Result<Option<usize>> {
    // One ask per part, so no prompt exceeds the typed-prompt limit; the
    // candidates are staged only when every part parsed, so a retry of the
    // window does not stage the earlier parts twice.
    let mut all: Vec<Candidate> = Vec::new();
    for prompt in metabolism_prompts(agent, body) {
        let raw = model.ask(&prompt).await?;
        let cleaned = strip_code_fence(&raw);
        match serde_json::from_str::<Vec<Candidate>>(&cleaned) {
            Ok(v) => all.extend(v),
            Err(e) => {
                tracing::warn!("metabolism: parse failed for {}: {} — raw: {}", agent, e, cleaned);
                return Ok(None);
            }
        }
    }
    if all.is_empty() {
        return Ok(Some(0));
    }
    append_pending(agent_dir, &all)?;
    Ok(Some(all.len()))
}

fn metabolism_prompt(agent: &str, body: &str) -> String {
    format!(r#"Read these recent diary entries from agent "{agent}". Identify candidates worth
promoting to long-term shared memory. A candidate is a stable user fact, a preference,
a piece of feedback, or a recurring observation — not a one-off task summary.

Output a JSON array (no markdown fences, no prose). Each element:
{{"tag": "FACT|FEEDBACK|OBSERVATION|NOTABLE", "body": "<one or two sentences>", "confidence": <0..1>}}

Empty array if nothing worth promoting.

Diary content:
---
{body}
---"#)
}

/// The metabolism prompts for one window of `body`: one prompt, or several
/// when the window is larger than one prompt may be.
fn metabolism_prompts(agent: &str, body: &str) -> Vec<String> {
    let budget = PROMPT_LIMIT.saturating_sub(metabolism_prompt(agent, "").len());
    split_to_budget(body, budget).iter().map(|part| metabolism_prompt(agent, part)).collect()
}

/// Largest prompt the distiller builds: the typed-prompt limit minus room
/// for the date preamble the session adds.
const PROMPT_LIMIT: usize = nucleus_core::claude_session::MAX_TYPED_PROMPT_BYTES - 1024;

/// Split `text` into parts of at most `max` bytes, at diary headings
/// (`## `/`### ` lines) where possible, else at line ends, else inside a
/// line (never inside a UTF-8 character). Pure.
fn split_to_budget(text: &str, max: usize) -> Vec<String> {
    let max = max.max(64);
    if text.len() <= max {
        return vec![text.to_string()];
    }
    // Sections: a heading line and the lines up to the next heading.
    let mut sections: Vec<String> = Vec::new();
    for line in text.split_inclusive('\n') {
        if line.starts_with("## ") || line.starts_with("### ") || sections.is_empty() {
            sections.push(String::new());
        }
        sections.last_mut().unwrap().push_str(line);
    }
    let mut pieces: Vec<String> = Vec::new();
    for sec in sections {
        if sec.len() <= max {
            pieces.push(sec);
            continue;
        }
        for line in sec.split_inclusive('\n') {
            if line.len() <= max {
                pieces.push(line.to_string());
                continue;
            }
            let mut cur = String::new();
            for c in line.chars() {
                if cur.len() + c.len_utf8() > max {
                    pieces.push(std::mem::take(&mut cur));
                }
                cur.push(c);
            }
            if !cur.is_empty() {
                pieces.push(cur);
            }
        }
    }
    let mut parts: Vec<String> = Vec::new();
    let mut cur = String::new();
    for p in pieces {
        if !cur.is_empty() && cur.len() + p.len() > max {
            parts.push(std::mem::take(&mut cur));
        }
        cur.push_str(&p);
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

fn contemplation_prompt(
    agent: &str,
    vault_path: &Path,
    vault_summary: &str,
    retain_days: u32,
    body: &str,
    pending: &str,
) -> String {
    format!(
        r#"You are the weekly distiller for agent "{agent}". Read the candidate
observations below and decide an op for each. The vault at {vault:?} is mounted
via --add-dir — read files freely when classifying.

Operations:
  PROMOTE — write a NEW file to Tier 2 (shared, auto-loaded into every claude
            session). Use for short, recurring, behaviorally-binding facts the
            bots need every spawn ("user prefers terse replies", "timezone
            <region/city>", "Discord home channel = X"). One fact per file.
  MERGE   — append to an EXISTING Tier 2 file under a dated `## Update` heading.
            `body` is the text that gets appended VERBATIM: write the new fact or
            evidence itself, never an instruction to an editor ('Add a section...',
            'Append to the existing body...') — nobody reads those, they get filed.
            Do not restate what the file already says.
  ARCHIVE — write a longer-form note to T3 (the user's PARA-organized Obsidian
            second brain). Use for narrative/decisions/notes the user might
            want to browse later, not facts the bot needs every spawn.
  DROP    — no action; explain why in `reason`.

T2 vs T3 split: the test is "does the bot need this in every spawn?" If yes,
PROMOTE/MERGE. If "user might want to browse this later," ARCHIVE.

ARCHIVE rules (CLAUDE.md Rule 9 — read it if you haven't):
  1. `bucket` MUST be one of:
       "0-Inbox"
       "1-Main-Notes"          (only if capture is explicitly hub/MOC)
       "2-Daily-Notes"         (only for time-anchored entries; name YYYY-MM-DD.md)
       "3-Projects/<existing>"
       "4-Areas/<existing>"
       "5-Resources/<existing>"
       "6-Slipbox"             (atomic evergreen ideas; flat, no sub-folders)
       "7-Archives/<...>"
     DO NOT invent new sub-folders under 3-Projects, 4-Areas, or
     5-Resources — those are the user's durable commitments and need
     human authorship. If you can't find a matching existing sub-folder,
     prefer "6-Slipbox" for atomic ideas or "0-Inbox" for unclassified.
  2. Read the per-bucket README.md to understand what belongs where.
  3. Read the immediate sibling notes in your chosen bucket and add
     [[wiki-links]] to thematically related ones in the body. Don't
     fabricate links to notes that don't exist.
  4. Body must start with YAML frontmatter:
     ---
     created: YYYY-MM-DD
     source: distiller-contemplation
     tags: [free-form-list-or-omit]
     ---
  5. `filename` is a leaf filename like "2026-W19-discord-routing.md"; the
     distiller writes it under `bucket/`. If None, defaults to
     `YYYY-Www-<agent>.md`.
  6. Prefer APPEND over a new note (CLAUDE.md Rule 9.4). Before choosing a
     new filename, search the vault for the theme:
       {vault_search} <2-4 distinctive words> [--bucket <bucket>]
     Each result prints a note's vault-relative path. If one already covers
     the theme, set `bucket` to its folder and `filename` to its file name:
     the distiller then appends your body to that note under a separator
     (leading frontmatter in the body is dropped when appending). Several
     dated notes on one theme in one folder mean the theme already has a
     note; append to the most recent one.

Vault structure right now (truncated folder tree; use the search above to find
notes by content):
{vault_summary}

PROMOTE / MERGE bodies are plain markdown: no YAML frontmatter (the distiller
writes the header from `name`/`description`/`kind`). Frontmatter belongs only in
ARCHIVE bodies.

Output a JSON array (no fences, no prose). Each element:
{{
  "op": "PROMOTE|MERGE|ARCHIVE|DROP",
  "name": "kebab-case-slug",                      // PROMOTE/MERGE
  "kind": "user|feedback|project|reference",      // PROMOTE
  "description": "one-line summary",              // PROMOTE
  "bucket": "4-Areas/Nucleus",                    // ARCHIVE
  "filename": "2026-W19-something.md",            // ARCHIVE (optional)
  "body": "markdown body with frontmatter",       // PROMOTE/MERGE/ARCHIVE
  "reason": "why this op"                         // always
}}

DIARY (last {retain_days} days):
---
{body}
---

PENDING CANDIDATES:
---
{pending}
---"#,
        vault = vault_path,
        vault_search = VAULT_SEARCH_CMD,
    )
}

/// The contemplation prompts for one agent, each with the number of bytes
/// of `pending` it covers (the parts are consecutive and together cover all
/// of `pending`). One prompt when everything fits; otherwise the pending
/// candidates are split into parts (each judged once), and each prompt
/// carries the most recent diary that fits beside its part.
fn contemplation_prompts(
    agent: &str,
    vault_path: &Path,
    vault_summary: &str,
    retain_days: u32,
    body: &str,
    pending: &str,
) -> Vec<(String, usize)> {
    let whole = contemplation_prompt(agent, vault_path, vault_summary, retain_days, body, pending);
    if whole.len() <= PROMPT_LIMIT {
        return vec![(whole, pending.len())];
    }
    let overhead = contemplation_prompt(agent, vault_path, vault_summary, retain_days, "", "").len();
    let budget = PROMPT_LIMIT.saturating_sub(overhead);
    split_to_budget(pending, budget / 2)
        .iter()
        .map(|part| {
            let diary = recent_within(body, budget.saturating_sub(part.len()));
            (contemplation_prompt(agent, vault_path, vault_summary, retain_days, &diary, part), part.len())
        })
        .collect()
}

/// The most recent part of `text` that fits in `max` bytes, cut at a
/// heading or line end, with a note when older content was left out. Pure.
fn recent_within(text: &str, max: usize) -> String {
    const NOTE: &str = "(older diary entries omitted to fit the prompt limit)\n";
    if text.len() <= max {
        return text.to_string();
    }
    let parts = split_to_budget(text, max.saturating_sub(NOTE.len()).max(64));
    let mut kept: Vec<&String> = Vec::new();
    let mut size = NOTE.len();
    for p in parts.iter().rev() {
        if size + p.len() > max {
            break;
        }
        size += p.len();
        kept.push(p);
    }
    kept.reverse();
    format!("{NOTE}{}", kept.into_iter().map(String::as_str).collect::<String>())
}

fn append_pending(agent_dir: &Path, candidates: &[Candidate]) -> Result<()> {
    let pending = agent_dir.join("_pending.md");
    let mut buf = String::new();
    let now = Local::now();
    buf.push_str(&format!("\n## {} — {} candidates\n", now.format("%Y-%m-%d %H:%M"), candidates.len()));
    for c in candidates {
        buf.push_str(&format!("- [{}] (conf {:.2}) {}\n", c.tag, c.confidence, c.body.trim()));
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create(true).append(true).open(&pending)?;
    f.write_all(buf.as_bytes())?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct Decision {
    op: String,                  // PROMOTE | MERGE | ARCHIVE | DROP
    name: Option<String>,        // memory file slug for PROMOTE/MERGE
    description: Option<String>, // for PROMOTE
    kind: Option<String>,        // user|feedback|project|reference for PROMOTE
    body: Option<String>,        // markdown body for PROMOTE/MERGE/ARCHIVE
    /// PARA destination for ARCHIVE: e.g. "4-Areas/Nucleus", "0-Inbox",
    /// "5-Resources/Rust-async", "6-Slipbox". Validated against the
    /// vault's allowed top-level buckets in [`resolve_bucket`]; new
    /// sub-folders under 3-Projects / 4-Areas / 5-Resources are NOT
    /// auto-created (those represent durable user commitments, see
    /// CLAUDE.md Rule 9).
    bucket: Option<String>,
    /// ARCHIVE filename within `bucket`. If None we generate
    /// `YYYY-Www-<agent>.md`. Should be a leaf filename (no path
    /// separators).
    filename: Option<String>,
    reason: Option<String>,      // for log
}

async fn contemplation(workspace_root: &Path, diary_root: &Path, settings: &Settings) -> Result<()> {
    let agents = list_agent_dirs(diary_root)?;
    if agents.is_empty() {
        tracing::info!("contemplation: no agent diaries found");
        return Ok(());
    }
    let vault_path = expand_home(&settings.obsidian.vault_path);
    // Resumes the metabolism session (ADR-029 daily session) — the vault
    // --add-dir is passed on every launch, so the resumed session has it.
    let (mut session, ask_opts) = SessionProfile::one_shot_utility(&ProfileContext {
        workspace_root,
        claude: &settings.claude,
        tmux_session: "nucleus-distiller",
        agent_label: "distiller",
    })
    .add_dirs(vec![vault_path.clone()])
    .window_name("contemplation")
    // Each ask may now run vault searches (ADR-035) before answering; the
    // utility profile's 180 s ceiling was sized for a single JSON reply.
    .max_wait(std::time::Duration::from_secs(360))
    .daily_session(AGENT_NAME)
    .spawn()
    .await
    .context("spawning claude session for contemplation")?;
    let week_ago = Local::now() - Duration::days(settings.diary.retain_days as i64);
    let vault_summary = summarize_vault(&vault_path);
    let mut applied_all = true;
    let mut judge = SessionJudge {
        model: SessionModel { session: &mut session, opts: &ask_opts },
        vault_path: vault_path.clone(),
    };
    let ctx = ContemplationContext {
        workspace_root,
        vault_path: &vault_path,
        vault_summary: &vault_summary,
        retain_days: settings.diary.retain_days,
    };

    for (agent, agent_dir) in &agents {
        if agent == AGENT_NAME { continue; }
        let body = read_recent_entries(agent_dir, week_ago)?;
        match contemplate_agent(&mut judge, &ctx, agent, agent_dir, &body).await? {
            Contemplated::Nothing => {}
            Contemplated::Incomplete => applied_all = false,
            Contemplated::Complete => {
                prune_old_diaries(agent_dir, week_ago.date_naive())?;
                clear_pending(workspace_root, agent, agent_dir).await?;
            }
        }
    }

    let _ = session.close().await;

    // Days with nothing but routine entries do not earn a diary file: drop
    // them for every agent, including this one, whose own diary the loop
    // above skips (it also never had its old days pruned — 105 files by
    // 2026-09-08).
    let mut dropped = 0usize;
    for (agent, agent_dir) in &agents {
        if agent == AGENT_NAME {
            prune_old_diaries(agent_dir, week_ago.date_naive())?;
        }
        let removed = diary::prune_routine_only_days(agent_dir)?;
        dropped += removed.len();
        for d in removed {
            tracing::info!("pruned routine-only diary {}/{}", agent, d);
        }
    }

    let _ = diary::record_observation(
        workspace_root,
        AGENT_NAME,
        "contemplation",
        &format!("processed {} agents; {} routine-only day(s) dropped", agents.len(), dropped),
        if applied_all && dropped == 0 { diary::Tag::Routine } else { diary::Tag::Observation },
    );
    Ok(())
}

/// A contemplation pass: asks the model and applies its decisions.
trait Judge: Model {
    async fn apply(&mut self, agent: &str, d: &Decision) -> Result<()>;
}

struct SessionJudge<'a> {
    model: SessionModel<'a>,
    vault_path: PathBuf,
}

impl Model for SessionJudge<'_> {
    async fn ask(&mut self, prompt: &str) -> Result<String> {
        self.model.ask(prompt).await
    }
}

impl Judge for SessionJudge<'_> {
    async fn apply(&mut self, agent: &str, d: &Decision) -> Result<()> {
        apply_decision(agent, d, &self.vault_path).await
    }
}

struct ContemplationContext<'a> {
    workspace_root: &'a Path,
    vault_path: &'a Path,
    vault_summary: &'a str,
    retain_days: u32,
}

#[derive(Debug, PartialEq, Eq)]
enum Contemplated {
    /// No diary and no pending candidates.
    Nothing,
    /// Every part was judged and every decision applied.
    Complete,
    /// A part did not parse or one of its decisions failed; the parts before
    /// it are recorded as done, it and the rest stay for the next run.
    Incomplete,
}

/// How much of an agent's `_pending.md` contemplation has applied: its first
/// `offset` bytes, whose SHA-256 is `prefix_sha256`. The hash detects a file
/// that was cleared (and possibly refilled) after the mark was written; such
/// a mark no longer applies and the file is read from the start. Stored as
/// `<offset>:<hex>`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingMark {
    offset: usize,
    prefix_sha256: String,
}

impl PendingMark {
    fn of(pending: &str, offset: usize) -> PendingMark {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(pending.as_bytes().get(..offset).unwrap_or_default());
        PendingMark { offset, prefix_sha256: digest.iter().map(|b| format!("{b:02x}")).collect() }
    }

    fn parse(v: &str) -> Option<PendingMark> {
        let (o, h) = v.split_once(':')?;
        Some(PendingMark { offset: o.parse().ok()?, prefix_sha256: h.to_string() })
    }

    fn encode(&self) -> String {
        format!("{}:{}", self.offset, self.prefix_sha256)
    }

    /// Bytes of `pending` already applied: the offset when the file still
    /// starts with the content the mark was taken over, else 0.
    fn applied_in(&self, pending: &str) -> usize {
        if self.offset <= pending.len()
            && pending.is_char_boundary(self.offset)
            && PendingMark::of(pending, self.offset) == *self
        {
            self.offset
        } else {
            0
        }
    }
}

/// Contemplation for one agent. The pending candidates are judged in parts
/// (see [`contemplation_prompts`]); after each part whose decisions all
/// applied, the progress mark moves past that part's bytes of
/// `_pending.md`, so a failure in a later part does not make the next run
/// judge and apply the earlier parts again. The first part that fails stops
/// the agent: the progress mark is a prefix, so a later part applied past a
/// failed one would be applied again when the failed one is retried. A part
/// with a failed decision is judged again whole on the next run (its other
/// decisions included); the model's decisions for a retried part are new
/// decisions, not a replay.
async fn contemplate_agent(
    judge: &mut impl Judge,
    ctx: &ContemplationContext<'_>,
    agent: &str,
    agent_dir: &Path,
    body: &str,
) -> Result<Contemplated> {
    let key = contemplation_progress_key(agent);
    let pending = std::fs::read_to_string(agent_dir.join("_pending.md")).unwrap_or_default();
    let mut done = match chore_state::watermark(ctx.workspace_root, &key).await? {
        Some(v) => PendingMark::parse(&v).map(|m| m.applied_in(&pending)).unwrap_or(0),
        None => 0,
    };
    let rest = &pending[done..];
    if body.trim().is_empty() && rest.trim().is_empty() {
        return Ok(Contemplated::Nothing);
    }

    let parts = contemplation_prompts(agent, ctx.vault_path, ctx.vault_summary, ctx.retain_days, body, rest);
    let mut counts = std::collections::HashMap::new();
    let mut outcome = Contemplated::Complete;
    for (prompt, consumed) in parts {
        let raw = judge.ask(&prompt).await?;
        let cleaned = strip_code_fence(&raw);
        let decisions: Vec<Decision> = match serde_json::from_str(&cleaned) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("contemplation: parse failed for {}: {} — raw: {}", agent, e, cleaned);
                outcome = Contemplated::Incomplete;
                break;
            }
        };
        let mut failed = 0usize;
        for d in &decisions {
            *counts.entry(d.op.clone()).or_insert(0) += 1;
            if let Err(e) = judge.apply(agent, d).await {
                failed += 1;
                tracing::warn!("contemplation: apply failed for {} {:?}: {}", agent, d.op, e);
            }
        }
        if failed > 0 {
            // A failed op keeps its inputs: this part's candidates stay
            // staged and the source diaries are not pruned, so the next pass
            // sees the same evidence again instead of losing it.
            tracing::warn!(
                "contemplation: agent {} — {} op(s) failed; keeping the rest of _pending.md and skipping the prune",
                agent, failed
            );
            outcome = Contemplated::Incomplete;
            break;
        }
        done += consumed;
        chore_state::set_watermark(ctx.workspace_root, &key, &PendingMark::of(&pending, done).encode()).await?;
    }
    tracing::info!("contemplation: agent {} → {:?}", agent, counts);
    Ok(outcome)
}

/// Empty `_pending.md` after a complete contemplation. The progress mark is
/// reset after the file: a crash in between leaves a mark whose hash no
/// longer matches the (empty or refilled) file, which [`PendingMark`]
/// treats as nothing applied.
async fn clear_pending(workspace_root: &Path, agent: &str, agent_dir: &Path) -> Result<()> {
    std::fs::write(agent_dir.join("_pending.md"), "")?;
    chore_state::set_watermark(workspace_root, &contemplation_progress_key(agent), &PendingMark::of("", 0).encode())
        .await
}

fn prune_old_diaries(agent_dir: &Path, before: NaiveDate) -> Result<()> {
    for e in std::fs::read_dir(agent_dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('_') || !name.ends_with(".md") { continue; }
        let stem = name.trim_end_matches(".md");
        if let Ok(d) = stem.parse::<NaiveDate>() {
            if d < before {
                let _ = std::fs::remove_file(e.path());
                tracing::info!("pruned old diary {:?}", e.path());
            }
        }
    }
    Ok(())
}

async fn apply_decision(agent: &str, d: &Decision, vault_path: &Path) -> Result<()> {
    match d.op.as_str() {
        "PROMOTE" | "MERGE" => {
            let name = d.name.clone().context("name required for PROMOTE/MERGE")?;
            let description = d.description.clone().unwrap_or_else(|| format!("from {}", agent));
            let body = d.body.clone().context("body required for PROMOTE/MERGE")?;
            let kind = match d.kind.as_deref().unwrap_or("reference") {
                "user" => memory::Kind::User,
                "feedback" => memory::Kind::Feedback,
                "project" => memory::Kind::Project,
                _ => memory::Kind::Reference,
            };
            let path = if d.op == "MERGE" {
                memory::merge(&name, &description, kind, &body, Local::now().date_naive())?
            } else {
                memory::promote(&memory::Memory { name: name.clone(), description, kind, body })?
            };
            tracing::info!("{}: {} {} -> {:?}", agent, d.op, name, path);
        }
        "ARCHIVE" => {
            let body = d.body.clone().context("body required for ARCHIVE")?;
            let path = archive_to_para(agent, vault_path, d.bucket.as_deref(), d.filename.as_deref(), &body)?;
            tracing::info!("{}: ARCHIVE {} chars -> {:?}", agent, body.len(), path);
        }
        "DROP" => {
            tracing::info!("{}: DROP — {}", agent, d.reason.clone().unwrap_or_default());
        }
        other => anyhow::bail!("unknown op: {}", other),
    }
    Ok(())
}

/// Write an ARCHIVE'd note into the user's PARA-organized vault under the
/// requested bucket. Falls back to `0-Inbox/` when [`resolve_bucket`]
/// rejects the bucket.
fn archive_to_para(
    agent: &str,
    vault_path: &Path,
    bucket: Option<&str>,
    filename: Option<&str>,
    body: &str,
) -> Result<PathBuf> {
    let resolved = resolve_bucket(vault_path, bucket).unwrap_or_else(|reason| {
        tracing::warn!(
            "ARCHIVE: bucket {:?} rejected ({}); falling back to 0-Inbox",
            bucket, reason
        );
        vault_path.join("0-Inbox")
    });
    std::fs::create_dir_all(&resolved)?;

    let leaf = filename
        .map(sanitize_filename)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let week = Local::now().format("%G-W%V");
            format!("{}-{}.md", week, agent)
        });
    let path = resolved.join(leaf);

    use std::io::Write;
    let exists = path.exists();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    // Appending to an existing note: the note already has its frontmatter,
    // so a second block in the middle of the file would be plain text.
    let body = if exists { strip_leading_frontmatter(body) } else { body };
    if exists {
        writeln!(f, "\n---\n")?;  // separator between appended sessions
    }
    f.write_all(body.trim().as_bytes())?;
    writeln!(f)?;
    Ok(path)
}

/// Resolve a Claude-supplied bucket string into an absolute path inside
/// the vault, OR return `Err(reason)` so the caller can fall back to
/// 0-Inbox.
///
/// Validation:
/// - Must start with one of the canonical top-level dirs (8 of them)
/// - Must not contain `..` or absolute path components
/// - For 3-Projects / 4-Areas / 5-Resources, the named sub-folder MUST
///   already exist (we don't auto-create durable user commitments).
///   0-Inbox, 1-Main-Notes, 2-Daily-Notes, 6-Slipbox, and 7-Archives/...
///   are allowed to create freely.
fn resolve_bucket(vault_path: &Path, bucket: Option<&str>) -> std::result::Result<PathBuf, String> {
    let raw = bucket.ok_or("no bucket supplied")?.trim().trim_matches('/');
    if raw.is_empty() {
        return Err("empty bucket".into());
    }
    if raw.contains("..") || raw.starts_with('/') {
        return Err(format!("path-escape attempt: {raw}"));
    }
    let top = raw.split('/').next().unwrap_or("");
    let allowed_tops = [
        "0-Inbox",
        "1-Main-Notes",
        "2-Daily-Notes",
        "3-Projects",
        "4-Areas",
        "5-Resources",
        "6-Slipbox",
        "7-Archives",
    ];
    if !allowed_tops.contains(&top) {
        return Err(format!("unknown top-level bucket: {top}"));
    }
    let target = vault_path.join(raw);
    let needs_existing_subdir = matches!(top, "3-Projects" | "4-Areas" | "5-Resources")
        && raw.contains('/');
    if needs_existing_subdir && !target.exists() {
        return Err(format!("sub-folder {raw} doesn't exist (won't auto-create)"));
    }
    Ok(target)
}

/// Strip path separators and a few other shenanigans from a Claude-supplied
/// filename. Doesn't enforce extension — Claude can pick `.md` or whatever.
fn sanitize_filename(name: impl AsRef<str>) -> String {
    name.as_ref()
        .chars()
        .filter(|c| !matches!(*c, '/' | '\\' | '\0'))
        .collect::<String>()
        .trim()
        .to_string()
}

/// Tilde-expand a config path.
fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(p)
    }
}

/// Compact tree summary of the vault's top three levels — fed to Claude
/// so it can pick a real `bucket` and know what siblings exist for linking.
/// Caps depth and breadth to keep the prompt reasonable.
fn summarize_vault(vault: &Path) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "{}/", vault.display());
    // Report a failed listing instead of silently yielding an empty summary.
    // An unsigned binary loses its Full Disk Access grant (ADR-030) and this
    // read comes back as a permission error; swallowing it made the whole vault
    // look empty with nothing in the log to say why.
    if let Err(e) = std::fs::read_dir(vault) {
        tracing::error!(
            path = %vault.display(), err = %e,
            "cannot list the vault — contemplation will run without vault structure. \
             If this is a permission error, the binary is probably unsigned: run ./tools/build.sh"
        );
    }
    let mut tops: Vec<_> = std::fs::read_dir(vault).into_iter()
        .flatten().flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter(|e| {
            let n = e.file_name();
            !n.to_string_lossy().starts_with('.')
        })
        .collect();
    tops.sort_by_key(|e| e.file_name());
    for top in tops {
        let top_name = top.file_name().to_string_lossy().into_owned();
        let _ = writeln!(out, "  {}/", top_name);
        let mut subs: Vec<_> = std::fs::read_dir(top.path()).into_iter()
            .flatten().flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .collect();
        subs.sort_by_key(|e| e.file_name());
        for sub in subs.iter().take(20) {
            let sub_name = sub.file_name().to_string_lossy().into_owned();
            let note_count = std::fs::read_dir(sub.path()).into_iter()
                .flatten().flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".md"))
                .count();
            let _ = writeln!(out, "    {}/  ({} notes)", sub_name, note_count);
        }
        if subs.len() > 20 {
            let _ = writeln!(out, "    … and {} more", subs.len() - 20);
        }
        // Also list top-level .md files in this bucket (notes filed directly).
        let mut top_notes: Vec<_> = std::fs::read_dir(top.path()).into_iter()
            .flatten().flatten()
            .filter(|e| {
                let n = e.file_name();
                n.to_string_lossy().ends_with(".md") && n.to_string_lossy() != "README.md"
            })
            .collect();
        top_notes.sort_by_key(|e| e.file_name());
        for n in top_notes.iter().take(10) {
            let _ = writeln!(out, "    {}", n.file_name().to_string_lossy());
        }
        if top_notes.len() > 10 {
            let _ = writeln!(out, "    … and {} more notes", top_notes.len() - 10);
        }
    }
    out
}

fn strip_code_fence(s: &str) -> String {
    let t = s.trim();
    let t = t.trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    t.to_string()
}

/// The body without a leading `---` frontmatter block.
fn strip_leading_frontmatter(body: &str) -> &str {
    let trimmed = body.trim_start();
    match nucleus_core::vault::note::split_frontmatter(trimmed) {
        (Some(_), rest, _) => rest,
        (None, _, _) => body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diary(days: usize, entry_bytes: usize) -> String {
        let mut out = String::new();
        for d in 0..days {
            out.push_str(&format!("### 2026-09-{:02}\n\n", d + 1));
            for i in 0..10 {
                out.push_str(&format!("## 10:{i:02} — OBSERVATION\n{}\n", "é".repeat(entry_bytes / 2)));
            }
        }
        out
    }

    #[test]
    fn split_keeps_everything_within_the_budget() {
        let text = diary(3, 4_000);
        let parts = split_to_budget(&text, 10_000);
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|p| p.len() <= 10_000));
        assert_eq!(parts.concat(), text, "nothing lost or reordered");
        // A single line longer than the budget is cut, never inside a char.
        let line = "ü".repeat(10_000);
        let parts = split_to_budget(&line, 1_001);
        assert!(parts.iter().all(|p| p.len() <= 1_001));
        assert_eq!(parts.concat(), line);
    }

    #[test]
    fn no_distiller_prompt_exceeds_the_typed_prompt_limit() {
        let cap = nucleus_core::claude_session::MAX_TYPED_PROMPT_BYTES;
        // A window of diary three times the limit is several metabolism asks.
        let body = diary(6, 4_000);
        assert!(body.len() > 3 * cap);
        let prompts = metabolism_prompts("discord", &body);
        assert!(prompts.len() >= 3);
        assert!(prompts.iter().all(|p| p.len() <= cap), "{:?}", prompts.iter().map(String::len).collect::<Vec<_>>());
        // Contemplation: large diary and large pending list.
        let pending: String = (0..4_000).map(|i| format!("- [FACT] (conf 0.80) candidate number {i} about something\n")).collect();
        let parts = contemplation_prompts("discord", Path::new("/vault"), "vault/\n", 7, &body, &pending);
        assert_eq!(parts.iter().map(|(_, n)| n).sum::<usize>(), pending.len(), "the parts cover the pending list");
        let prompts: Vec<String> = parts.into_iter().map(|(p, _)| p).collect();
        assert!(prompts.len() > 1);
        assert!(prompts.iter().all(|p| p.len() <= cap));
        for i in [0, 1_999, 3_999] {
            let line = format!("candidate number {i} about something");
            assert_eq!(prompts.iter().filter(|p| p.contains(&line)).count(), 1, "each candidate is judged once");
        }
        assert!(prompts.iter().all(|p| p.contains("older diary entries omitted")));
        // Small inputs stay one prompt with the whole diary.
        let small = contemplation_prompts("discord", Path::new("/vault"), "vault/\n", 7, "### d\nx\n", "- [FACT] y\n");
        assert_eq!(small.len(), 1);
        assert!(!small[0].0.contains("omitted"));
    }

    fn tmp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "distiller-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Records every prompt; replies with a fixed script (default `[]`).
    #[derive(Default)]
    struct Fake {
        prompts: Vec<String>,
        replies: std::collections::VecDeque<String>,
        applied: Vec<String>,
        fail_apply: Vec<String>,
    }

    impl Model for Fake {
        async fn ask(&mut self, prompt: &str) -> Result<String> {
            self.prompts.push(prompt.to_string());
            Ok(self.replies.pop_front().unwrap_or_else(|| "[]".into()))
        }
    }

    impl Judge for Fake {
        async fn apply(&mut self, _agent: &str, d: &Decision) -> Result<()> {
            let name = d.name.clone().unwrap_or_default();
            if self.fail_apply.contains(&name) {
                anyhow::bail!("apply failed");
            }
            self.applied.push(name);
            Ok(())
        }
    }

    fn entry(agent_dir: &Path, date: NaiveDate, text: &str) {
        use std::io::Write;
        let path = agent_dir.join(format!("{date}.md"));
        let fresh = !path.exists();
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).unwrap();
        if fresh {
            write!(f, "---\nagent: x\ndate: {date}\n---\n").unwrap();
        }
        write!(f, "\n## 10:00 — dm\n{text}\n- OBSERVATION: {text}\n").unwrap();
    }

    /// Two runs on the same day with the diary growing in between, then a
    /// run the next day: every diary entry reaches the model exactly once.
    #[tokio::test]
    async fn metabolism_sends_each_diary_entry_once_across_runs() {
        let root = tmp_dir("metab");
        let agent_dir = root.join("memory/diaries/discord");
        std::fs::create_dir_all(&agent_dir).unwrap();
        let d = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        entry(&agent_dir, d.pred_opt().unwrap(), "entry-yesterday");
        entry(&agent_dir, d, "entry-alpha");

        let mut fake = Fake::default();
        metabolize_agent(&mut fake, &root, "discord", &agent_dir, d).await.unwrap();
        entry(&agent_dir, d, "entry-beta");
        metabolize_agent(&mut fake, &root, "discord", &agent_dir, d).await.unwrap();
        // Nothing new: no ask at all.
        let asks = fake.prompts.len();
        metabolize_agent(&mut fake, &root, "discord", &agent_dir, d).await.unwrap();
        assert_eq!(fake.prompts.len(), asks, "a run with no new diary asks nothing");
        let next = d.succ_opt().unwrap();
        entry(&agent_dir, next, "entry-gamma");
        metabolize_agent(&mut fake, &root, "discord", &agent_dir, next).await.unwrap();

        let all = fake.prompts.concat();
        for e in ["entry-yesterday", "entry-alpha", "entry-beta", "entry-gamma"] {
            // Each entry appears twice in its prompt (heading body + bullet).
            assert_eq!(all.matches(&format!("\n{e}\n")).count(), 1, "{e} sent once: {:#?}", fake.prompts);
        }
        assert_eq!(fake.prompts.len(), 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn diary_mark_reads_the_earlier_date_format() {
        let d = NaiveDate::from_ymd_opt(2026, 9, 10).unwrap();
        assert_eq!(DiaryMark::parse("2026-09-10"), Some(DiaryMark { date: d.succ_opt().unwrap(), offset: 0 }));
        let m = DiaryMark { date: d, offset: 123 };
        assert_eq!(DiaryMark::parse(&m.encode()), Some(m));
        assert_eq!(DiaryMark::parse("junk"), None);
    }

    fn decision(name: &str) -> String {
        format!(r#"{{"op": "DROP", "name": "{name}", "reason": "r"}}"#)
    }

    /// A contemplation whose second part fails: the next run judges the
    /// second part and after, never the first part again.
    #[tokio::test]
    async fn contemplation_resumes_after_the_last_applied_part() {
        let root = tmp_dir("contemp");
        let agent_dir = root.join("memory/diaries/discord");
        std::fs::create_dir_all(&agent_dir).unwrap();
        // Enough candidates for several parts.
        let pending: String = (0..4_000).map(|i| format!("- [FACT] (conf 0.80) candidate number {i:04} about something\n")).collect();
        std::fs::write(agent_dir.join("_pending.md"), &pending).unwrap();
        let ctx = ContemplationContext {
            workspace_root: &root,
            vault_path: Path::new("/vault"),
            vault_summary: "vault/\n",
            retain_days: 7,
        };
        let parts = contemplation_prompts("discord", ctx.vault_path, ctx.vault_summary, 7, "", &pending);
        assert!(parts.len() >= 3, "{}", parts.len());

        // Run 1: part 1 applies; part 2 has a decision that fails.
        let mut fake = Fake::default();
        fake.replies.push_back(format!("[{}]", decision("part1")));
        fake.replies.push_back(format!("[{}, {}]", decision("part2-ok"), decision("part2-bad")));
        fake.fail_apply.push("part2-bad".into());
        let out = contemplate_agent(&mut fake, &ctx, "discord", &agent_dir, "").await.unwrap();
        assert_eq!(out, Contemplated::Incomplete);
        assert_eq!(fake.prompts.len(), 2, "stops at the failed part");
        assert!(fake.prompts[0].contains("candidate number 0000"));

        // Run 2: resumes at part 2; part 1 is not judged or applied again.
        let mut fake2 = Fake::default();
        let out = contemplate_agent(&mut fake2, &ctx, "discord", &agent_dir, "").await.unwrap();
        assert_eq!(out, Contemplated::Complete);
        assert_eq!(fake2.prompts.len(), parts.len() - 1);
        assert!(fake2.prompts.iter().all(|p| !p.contains("candidate number 0000")), "part 1 not judged again");
        assert!(fake2.prompts[0].contains(&parts[1].0[parts[1].0.find("PENDING CANDIDATES").unwrap()..]));
        let last = "candidate number 3999";
        assert_eq!(fake2.prompts.iter().filter(|p| p.contains(last)).count(), 1);

        // After a complete run the file and the mark reset together; new
        // candidates staged later are judged from the start.
        clear_pending(&root, "discord", &agent_dir).await.unwrap();
        std::fs::write(agent_dir.join("_pending.md"), "- [FACT] (conf 0.9) fresh candidate\n").unwrap();
        let mut fake3 = Fake::default();
        contemplate_agent(&mut fake3, &ctx, "discord", &agent_dir, "").await.unwrap();
        assert!(fake3.prompts[0].contains("fresh candidate"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A mark left by a crash between clearing `_pending.md` and resetting
    /// the mark does not skip candidates staged afterwards.
    #[test]
    fn a_stale_pending_mark_applies_to_nothing() {
        let old = "- [FACT] a\n- [FACT] b\n";
        let mark = PendingMark::of(old, old.len());
        assert_eq!(mark.applied_in(old), old.len());
        assert_eq!(mark.applied_in(&format!("{old}- [FACT] c\n")), old.len(), "appends keep the mark");
        assert_eq!(mark.applied_in("- [FACT] new one, longer than before\n"), 0);
        assert_eq!(mark.applied_in(""), 0);
    }

    #[test]
    fn archive_appends_without_a_second_frontmatter() {
        let tmp = std::env::temp_dir().join(format!("distiller-archive-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(tmp.join("6-Slipbox")).unwrap();
        let first = "---\ncreated: 2026-01-01\nsource: distiller-contemplation\n---\n# Idea\n\nFirst.";
        let p = archive_to_para("a", &tmp, Some("6-Slipbox"), Some("idea.md"), first).unwrap();
        let second = "---\ncreated: 2026-01-08\nsource: distiller-contemplation\n---\nSecond.";
        archive_to_para("a", &tmp, Some("6-Slipbox"), Some("idea.md"), second).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(text.matches("created:").count(), 1, "{text}");
        assert!(text.starts_with("---\ncreated: 2026-01-01"));
        assert!(text.trim_end().ends_with("Second."));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn strip_leading_frontmatter_keeps_plain_bodies() {
        assert_eq!(strip_leading_frontmatter("plain\n---\nnot fm"), "plain\n---\nnot fm");
        assert_eq!(strip_leading_frontmatter("---\na: b\n---\nbody"), "body");
    }
}
