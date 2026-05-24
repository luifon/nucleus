//! Long-lived interactive `claude` sessions driven via tmux.
//!
//! Architecture:
//! - We spawn `claude` (NOT `claude -p`) inside a tmux window. The Max
//!   subscription covers interactive usage; `-p` is moving to API-only.
//! - User messages go in via tmux's paste buffer (handles any content
//!   without shell-escape hell).
//! - Responses come out by tailing the session transcript file that
//!   claude writes at `$HOME/.claude/projects/<encoded-cwd>/<session-id>.jsonl`.
//!   No TUI scraping — the transcript is structured JSON, one event per line.
//! - "Done responding" is detected by quiescence: no new transcript lines for
//!   `quiescent_window` after at least one assistant event arrived. Tool
//!   calls and intermediate events are tolerated; we return the *last*
//!   assistant text block seen.
//!
//! The tmux window stays alive across many `ask()` calls — that's the whole
//! point. Closing happens via `Session::close()` or by killing the window
//! externally.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::process::Command;

use crate::claude::PermissionMode;

/// A live tmux-hosted claude session.
pub struct Session {
    pub session_id: String,
    /// tmux target like `session:window`. Used as the `-t` value for every command.
    pub tmux_target: String,
    /// Path to the claude transcript file we tail.
    pub transcript_path: PathBuf,
    /// Byte offset into the transcript; we only read past this on each `ask`.
    cursor: u64,
    /// Run-log bookkeeping (ADR-016). Set when spawned with an `agent_label`;
    /// drives the `runs.jsonl` start/end rows. `None` = no run-log.
    agent_label: Option<String>,
    /// Unique id for this spawn's run-log row (distinct from `session_id`).
    run_id: String,
    /// Workspace root, needed to resolve the `memory/logs/<agent>/` path.
    workspace_root: PathBuf,
}

/// Options for spawning a new claude session in tmux.
pub struct SpawnOptions {
    /// CWD that claude is launched from. Determines auto-memory + .claude/ resolution.
    pub workspace_root: PathBuf,
    /// Optional persona / instructions prepended to the system prompt.
    pub append_system_prompt: Option<String>,
    /// Permission mode (default: claude's own default).
    pub permission_mode: Option<PermissionMode>,
    /// Tool patterns to refuse.
    pub disallowed_tools: Vec<String>,
    /// Tool patterns pre-approved without prompting the auto-mode
    /// classifier — passed through as `--allowed-tools`. Use for MCP
    /// tools the persona is expected to call repeatedly (e.g.
    /// `mcp__claude_ai_Google_Calendar__create_event` for JARVIS).
    pub allowed_tools: Vec<String>,
    /// Extra dirs claude is allowed to touch (`--add-dir`).
    pub add_dirs: Vec<PathBuf>,
    /// tmux session name (e.g. "nucleus-discord"). Created if missing.
    pub tmux_session: String,
    /// tmux window name. Defaults to the first 8 chars of the session UUID.
    pub window_name: Option<String>,
    /// How long to wait for the transcript file to appear after launching claude.
    pub ready_timeout: Duration,
    /// If `Some(uuid)`, resume that existing claude session. If `None`, a
    /// fresh UUID is generated and a new session is started.
    pub resume_session_id: Option<String>,
    /// Registry agent name (ADR-016). When `Some`, each spawn appends a row
    /// to `memory/logs/<agent>/runs.jsonl` pointing at the transcript, so the
    /// raw output survives the window being killed. `None` = no run-log
    /// (examples, tests, ad-hoc sessions).
    pub agent_label: Option<String>,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            workspace_root: PathBuf::from("."),
            append_system_prompt: None,
            permission_mode: None,
            disallowed_tools: vec![],
            allowed_tools: vec![],
            add_dirs: vec![],
            tmux_session: "nucleus".into(),
            window_name: None,
            ready_timeout: Duration::from_secs(20),
            resume_session_id: None,
            agent_label: None,
        }
    }
}

/// Tunables for `Session::ask`.
pub struct AskOptions {
    /// Hard ceiling on how long a single ask may take.
    pub max_wait: Duration,
    /// "Stopped writing for this long" → consider claude done.
    pub quiescent_window: Duration,
}

impl Default for AskOptions {
    fn default() -> Self {
        Self {
            max_wait: Duration::from_secs(180),
            quiescent_window: Duration::from_secs(3),
        }
    }
}

impl Session {
    /// Spawn `claude` interactively inside a tmux window, returning a handle
    /// once the transcript file exists (i.e. claude has booted enough to
    /// accept input).
    pub async fn spawn(opts: SpawnOptions) -> Result<Self> {
        let (session_id, resuming) = match opts.resume_session_id.clone() {
            Some(id) => (id, true),
            None => (uuid::Uuid::new_v4().to_string(), false),
        };
        let window_name = opts
            .window_name
            .clone()
            .unwrap_or_else(|| session_id.chars().take(8).collect());

        let claude_args = build_claude_args(&session_id, resuming, &opts);
        let inner = format!(
            "cd {} && claude {}",
            shell_quote(&opts.workspace_root.to_string_lossy()),
            claude_args.iter().map(|a| shell_quote(a)).collect::<Vec<_>>().join(" ")
        );

        ensure_tmux_session(&opts.tmux_session).await?;
        let target = format!("{}:{}", opts.tmux_session, window_name);

        let out = Command::new("tmux")
            .args(["new-window", "-t", &opts.tmux_session, "-n", &window_name, &inner])
            .output()
            .await
            .context("tmux new-window")?;
        if !out.status.success() {
            anyhow::bail!(
                "tmux new-window failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }

        let transcript_path = transcript_path_for(&opts.workspace_root, &session_id);
        // First-time visits to a cwd show a "trust this folder?" prompt that
        // blocks claude from booting. Watch the pane and dismiss it if it appears.
        // The two TUI-readiness steps below can fail (timeout, blocked at an
        // interactive prompt we can't auto-dismiss); any such failure must
        // kill the tmux window we just created, otherwise repeated retries
        // leak windows — three accumulated during one chat outage when this
        // path used to bail without cleanup.
        if let Err(e) = dismiss_trust_prompt_if_present(&target, Duration::from_secs(5)).await {
            let _ = Command::new("tmux")
                .args(["kill-window", "-t", &target])
                .output()
                .await;
            return Err(e);
        }
        // Claude only creates the transcript file when it gets its first
        // message. Wait instead for the TUI to render the input prompt — at
        // that point send-keys is safe.
        if let Err(e) = wait_for_tui_ready(&target, opts.ready_timeout).await {
            let _ = Command::new("tmux")
                .args(["kill-window", "-t", &target])
                .output()
                .await;
            return Err(e);
        }
        // Small extra beat for cursor positioning.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // CRITICAL: when --resume'ing, the transcript file already has all
        // the prior turns. If we start reading from offset 0, wait_for_assistant
        // sees those as "current" content, marks haveAssistant=true on the
        // first poll, then triggers the quiescent extractor after 3s of
        // (silent) new-bytes-waiting — pulling the LAST historical assistant
        // text instead of the response to the current ask. Pin the cursor to
        // the file's current size at spawn time so we only ever consider
        // content appended AFTER this Session was created.
        let cursor = if resuming {
            tokio::fs::metadata(&transcript_path)
                .await
                .map(|m| m.len())
                .unwrap_or(0)
        } else {
            0
        };

        // Run-log: append an in-flight row pointing at the transcript so the
        // raw output is recoverable after the window is killed (ADR-016).
        // Best-effort — never fail a spawn because the index couldn't write.
        let run_id = uuid::Uuid::new_v4().to_string();
        if let Some(agent) = &opts.agent_label {
            let row = crate::runlog::RunRow {
                run_id: run_id.clone(),
                agent: agent.clone(),
                session_id: session_id.clone(),
                transcript_path: transcript_path.to_string_lossy().into_owned(),
                tmux_target: target.clone(),
                started_at: chrono::Utc::now().to_rfc3339(),
                ended_at: None,
                ok: None,
            };
            if let Err(e) = crate::runlog::record_start(&opts.workspace_root, &row) {
                tracing::warn!("run-log record_start failed for {agent}: {e:#}");
            }
        }

        Ok(Self {
            session_id,
            tmux_target: target,
            transcript_path,
            cursor,
            agent_label: opts.agent_label.clone(),
            run_id,
            workspace_root: opts.workspace_root.clone(),
        })
    }

    /// Send a user message and wait for claude's next assistant reply. Blocks
    /// for at most `opts.max_wait`.
    pub async fn ask(&mut self, message: &str, opts: AskOptions) -> Result<String> {
        // Wait for the transcript to be quiet for `quiescent_window` before
        // snapshotting the cursor. Claude can keep writing after the
        // previous `wait_for_assistant` returned (late tool outputs,
        // continuation tokens), and without this settle the next ask reads
        // from a stale offset and returns trailing content from the prior
        // turn — the classic "one message late" symptom.
        let from = wait_for_transcript_quiet(&self.transcript_path, opts.quiescent_window)
            .await
            .unwrap_or(self.cursor);
        let payload = with_date_preamble(message);
        paste_and_send(&self.tmux_target, &payload).await?;
        let reply = wait_for_assistant(
            &self.transcript_path,
            from,
            opts.max_wait,
            opts.quiescent_window,
        )
        .await?;
        self.cursor = tokio::fs::metadata(&self.transcript_path)
            .await
            .map(|m| m.len())
            .unwrap_or(self.cursor);
        Ok(reply)
    }

    /// Kill the tmux window (and the claude inside it).
    ///
    /// Finalizes the run-log row (sets `ended_at`, `ok = true`) — `ok` means
    /// "closed cleanly", which is true on every explicit close. A run that
    /// dies without `close()` (crash, SIGKILL) leaves `ended_at` null, which
    /// reads as "ran, outcome unknown". Error-tracking proper is out of scope
    /// (ADR-016): for scheduled agents the launchd exit code carries it; for
    /// fires the diary + ⚠️ alert do.
    pub async fn close(self) -> Result<()> {
        if let Some(agent) = &self.agent_label {
            if let Err(e) =
                crate::runlog::record_end(&self.workspace_root, agent, &self.run_id, true)
            {
                tracing::warn!("run-log record_end failed for {agent}: {e:#}");
            }
        }
        let _ = Command::new("tmux")
            .args(["kill-window", "-t", &self.tmux_target])
            .output()
            .await;
        Ok(())
    }
}

// ---- pool ----

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, RwLock};

/// Configuration shared across all sessions in a `SessionPool`.
#[derive(Clone)]
pub struct PoolConfig {
    pub workspace_root: PathBuf,
    pub append_system_prompt: Option<String>,
    pub permission_mode: Option<PermissionMode>,
    pub disallowed_tools: Vec<String>,
    pub allowed_tools: Vec<String>,
    pub add_dirs: Vec<PathBuf>,
    pub tmux_session: String,
    /// Sessions idle for longer than this get reaped on the next reap_idle()
    /// call. Set generously — re-spawning costs ~5s.
    pub idle_timeout: Duration,
    /// Registry agent name for the run-log (ADR-016); threaded into every
    /// session this pool spawns. `None` = no run-log.
    pub agent_label: Option<String>,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            workspace_root: PathBuf::from("."),
            append_system_prompt: None,
            permission_mode: None,
            disallowed_tools: vec![],
            allowed_tools: vec![],
            add_dirs: vec![],
            tmux_session: "nucleus".into(),
            idle_timeout: Duration::from_secs(60 * 60 * 4), // 4h
            agent_label: None,
        }
    }
}

/// Manages a `HashMap<chat_key, Session>`. Each chat keeps its own long-lived
/// claude in its own tmux window. Spawning is on-demand; closing is either
/// explicit or via `reap_idle`.
///
/// `chat_key` is whatever string identifies the conversation to the caller —
/// Discord channel ID, WhatsApp chat JID, "news-fetcher", etc. The pool
/// uses it both as the HashMap key and (truncated to 8 chars) as the tmux
/// window name.
pub struct SessionPool {
    config: PoolConfig,
    entries: Arc<RwLock<HashMap<String, Arc<Mutex<Entry>>>>>,
}

struct Entry {
    session: Session,
    last_active: Instant,
}

/// Result of a `SessionPool::ask` call.
pub struct AskResult {
    pub reply: String,
    pub session_id: String,
    pub elapsed: Duration,
    /// True if a fresh session was spawned for this call.
    pub was_cold_spawn: bool,
}

impl SessionPool {
    pub fn new(config: PoolConfig) -> Self {
        Self {
            config,
            entries: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Send `message` on behalf of `chat_key`. Spawns a session if one isn't
    /// already live for that key. If `resume_session_id` is supplied AND no
    /// live session exists, the spawned session resumes that prior conversation.
    pub async fn ask(
        &self,
        chat_key: &str,
        message: &str,
        resume_session_id: Option<String>,
        ask_opts: AskOptions,
    ) -> Result<AskResult> {
        let t0 = Instant::now();

        // Get-or-create the per-chat entry. We hold the write lock briefly,
        // then release before doing the actual work (which holds only the
        // entry's own mutex).
        let entry = {
            let mut entries = self.entries.write().await;
            if let Some(existing) = entries.get(chat_key).cloned() {
                existing
            } else {
                let window_name = sanitize_window_name(chat_key);
                let session = Session::spawn(SpawnOptions {
                    workspace_root: self.config.workspace_root.clone(),
                    append_system_prompt: self.config.append_system_prompt.clone(),
                    permission_mode: self.config.permission_mode,
                    disallowed_tools: self.config.disallowed_tools.clone(),
                    allowed_tools: self.config.allowed_tools.clone(),
                    add_dirs: self.config.add_dirs.clone(),
                    tmux_session: self.config.tmux_session.clone(),
                    window_name: Some(window_name),
                    ready_timeout: Duration::from_secs(20),
                    resume_session_id,
                    agent_label: self.config.agent_label.clone(),
                })
                .await?;
                let entry = Arc::new(Mutex::new(Entry {
                    session,
                    last_active: Instant::now(),
                }));
                entries.insert(chat_key.to_string(), entry.clone());
                entry
            }
        };

        let was_cold = t0.elapsed() > Duration::from_secs(2);
        let mut guard = entry.lock().await;
        let reply = guard.session.ask(message, ask_opts).await?;
        let session_id = guard.session.session_id.clone();
        guard.last_active = Instant::now();

        Ok(AskResult {
            reply,
            session_id,
            elapsed: t0.elapsed(),
            was_cold_spawn: was_cold,
        })
    }

    /// Drop sessions idle longer than `config.idle_timeout` and kill their
    /// tmux windows. Safe to call from a background task on a timer.
    pub async fn reap_idle(&self) -> Result<usize> {
        let now = Instant::now();
        let idle_threshold = self.config.idle_timeout;
        let mut to_close = Vec::new();
        {
            let entries = self.entries.read().await;
            for (key, entry) in entries.iter() {
                let guard = entry.lock().await;
                if now.duration_since(guard.last_active) > idle_threshold {
                    to_close.push(key.clone());
                }
            }
        }
        if to_close.is_empty() {
            return Ok(0);
        }
        let mut entries = self.entries.write().await;
        for key in &to_close {
            if let Some(entry) = entries.remove(key) {
                if let Ok(unwrapped) = Arc::try_unwrap(entry) {
                    let inner = unwrapped.into_inner();
                    let _ = inner.session.close().await;
                }
            }
        }
        Ok(to_close.len())
    }

    /// Roll every active per-chat session forward by one day: ask each one
    /// to summarize itself, append the summary to the agent's daily diary,
    /// spawn a fresh session primed with the summary + last 10 turns, and
    /// hand the new session-id back to the caller so it can persist the
    /// new chat-key → session-id mapping.
    ///
    /// Motivation: long-lived sessions eventually hit the "Resume from
    /// summary?" picker on resume, whose in-line compaction blows past our
    /// 20s TUI-ready timeout. Doing the compaction offline at 4am keeps
    /// every user-facing ask() out of that path.
    ///
    /// Chats are processed sequentially (one at a time per pool) — three
    /// minutes of summarization + spawn cost across a handful of chats is
    /// fine at 4am, and serialization avoids hammering the tmux server.
    ///
    /// Skips:
    /// - Chats whose `last_active` is more than 24h old (the idle reaper
    ///   will close them; no continuity worth preserving).
    /// - Chats with fewer than 10 text turns on the transcript (too small
    ///   to be worth rotating).
    ///
    /// Failure handling: a per-chat failure is recorded in the agent's
    /// diary with an Observation tag and the rotation moves on. The old
    /// session stays in place; the auto-dismiss picker safety net handles
    /// the picker if it appears before tomorrow's rotation retries.
    ///
    /// `db_update_session_id` receives `(chat_key, new_session_id)` once
    /// the new session is fully primed and ready. Order it so a partial
    /// rotation leaves either the old or new session id valid in your DB,
    /// never a dangling one (we close the old window only after the
    /// callback succeeds).
    pub async fn daily_rotate<F, Fut>(
        &self,
        agent_name: &str,
        mut db_update_session_id: F,
    ) -> RotationStats
    where
        F: FnMut(String, String) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let mut stats = RotationStats::default();
        let keys: Vec<(String, Arc<Mutex<Entry>>)> = {
            let entries = self.entries.read().await;
            entries
                .iter()
                .map(|(k, e)| (k.clone(), e.clone()))
                .collect()
        };
        for (chat_key, entry_arc) in keys {
            stats.considered += 1;
            match self
                .rotate_one(&chat_key, &entry_arc, agent_name, &mut db_update_session_id)
                .await
            {
                Ok(RotateOutcome::Rotated) => stats.rotated += 1,
                Ok(RotateOutcome::Skipped) => stats.skipped += 1,
                Err(e) => {
                    stats.failed += 1;
                    let _ = crate::diary::record_observation(
                        &self.config.workspace_root,
                        agent_name,
                        &format!("daily_rotate {}", chat_key),
                        &format!("rotation failed: {e:#}"),
                        crate::diary::Tag::Observation,
                    );
                }
            }
        }
        stats
    }

    async fn rotate_one<F, Fut>(
        &self,
        chat_key: &str,
        entry_arc: &Arc<Mutex<Entry>>,
        agent_name: &str,
        db_update_session_id: &mut F,
    ) -> Result<RotateOutcome>
    where
        F: FnMut(String, String) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let mut guard = entry_arc.lock().await;

        // Skip cold chats — the idle reaper will close them and there's no
        // user continuity worth preserving.
        if guard.last_active.elapsed() > Duration::from_secs(24 * 60 * 60) {
            return Ok(RotateOutcome::Skipped);
        }

        let transcript_path = guard.session.transcript_path.clone();
        let turns = last_n_turns(&transcript_path, 100);
        if turns.len() < 10 {
            return Ok(RotateOutcome::Skipped);
        }
        let replay: Vec<Turn> = turns.iter().rev().take(10).rev().cloned().collect();

        // Step 1: ask the old session to summarize itself. Generous timeout
        // — no user is waiting and the model needs to read the whole
        // transcript. If this triggers the picker, the existing auto-dismiss
        // path handles it.
        let summary = guard
            .session
            .ask(SUMMARY_PROMPT, AskOptions {
                max_wait: Duration::from_secs(300),
                quiescent_window: Duration::from_secs(5),
            })
            .await
            .context("daily_rotate: summary ask failed")?;

        // Step 2: append the summary to today's diary.
        let entry = crate::diary::Entry::now(
            format!("daily_rotate {}", chat_key),
            format!("Session rotated. Yesterday's summary:\n\n{}", summary.trim()),
        );
        let _ = crate::diary::append(&self.config.workspace_root, agent_name, &entry);

        // Step 3: spawn a brand-new session (no --resume, fresh UUID).
        // Window name derived from the new UUID so it doesn't collide with
        // the still-alive old window — we'll kill the old one after the DB
        // update succeeds.
        let new_session = Session::spawn(SpawnOptions {
            workspace_root: self.config.workspace_root.clone(),
            append_system_prompt: self.config.append_system_prompt.clone(),
            permission_mode: self.config.permission_mode,
            disallowed_tools: self.config.disallowed_tools.clone(),
            allowed_tools: self.config.allowed_tools.clone(),
            add_dirs: self.config.add_dirs.clone(),
            tmux_session: self.config.tmux_session.clone(),
            window_name: None,
            ready_timeout: Duration::from_secs(60),
            resume_session_id: None,
            agent_label: self.config.agent_label.clone(),
        })
        .await
        .context("daily_rotate: spawn new session")?;
        let new_session_id = new_session.session_id.clone();

        // Step 4: prime the new session with the summary + last 10 turns.
        // If priming fails we tear down the new session so we don't orphan it.
        let priming = build_priming_preamble(&summary, &replay);
        let mut new_session = new_session;
        if let Err(e) = new_session
            .ask(&priming, AskOptions {
                max_wait: Duration::from_secs(300),
                quiescent_window: Duration::from_secs(5),
            })
            .await
        {
            let _ = new_session.close().await;
            return Err(e).context("daily_rotate: prime new session");
        }

        // Step 5: hand the new session-id to the caller so the DB row is
        // updated BEFORE we close the old session. If this fails we tear
        // down the new session — old one stays the source of truth.
        if let Err(e) = db_update_session_id(chat_key.to_string(), new_session_id.clone()).await {
            let _ = new_session.close().await;
            return Err(e).context("daily_rotate: db update");
        }

        // Step 6: swap in the new session, then close the old one.
        let old_session = std::mem::replace(&mut guard.session, new_session);
        let _ = old_session.close().await;
        guard.last_active = Instant::now();

        Ok(RotateOutcome::Rotated)
    }

    /// Close every session and tear down the tmux session.
    pub async fn shutdown(&self) -> Result<()> {
        let mut entries = self.entries.write().await;
        let keys: Vec<String> = entries.keys().cloned().collect();
        for key in keys {
            if let Some(entry) = entries.remove(&key) {
                if let Ok(unwrapped) = Arc::try_unwrap(entry) {
                    let inner = unwrapped.into_inner();
                    let _ = inner.session.close().await;
                }
            }
        }
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &self.config.tmux_session])
            .output()
            .await;
        Ok(())
    }
}

/// Outcome of rotating a single chat. Aggregated into [`RotationStats`].
enum RotateOutcome {
    Rotated,
    Skipped,
}

/// Roll-up from one [`SessionPool::daily_rotate`] pass. Callers log this
/// for observability — the counts answer "did the 4am tick do anything?"
#[derive(Debug, Default)]
pub struct RotationStats {
    pub considered: usize,
    pub rotated: usize,
    pub skipped: usize,
    pub failed: usize,
}

/// The deterministic prompt the old session is given before being closed.
/// Phrased so the reply is the bullets directly, with no surrounding
/// preamble we'd then have to strip before writing to the diary.
const SUMMARY_PROMPT: &str =
    "Summarize this conversation in 5-10 bullets for tomorrow's session. \
     Focus on ongoing tasks, decisions made, key facts about the user, \
     and anything a fresh assistant would need to know. \
     Reply with only the bullets.";

/// Build the first message a freshly-rotated session sees: yesterday's
/// summary on top, then the last N turns replayed in plain "USER:"/
/// "ASSISTANT:" form for textual continuity. Audio attachments are not
/// included specially — they've already been transcribed by the time
/// the turn lands in the transcript JSONL.
fn build_priming_preamble(summary: &str, replay: &[Turn]) -> String {
    let mut out = String::new();
    out.push_str("[Yesterday's session summary, for context]\n");
    out.push_str(summary.trim());
    out.push_str("\n\n[Recent conversation, replayed for continuity]\n");
    for turn in replay {
        let label = match turn.role {
            TurnRole::User => "USER",
            TurnRole::Assistant => "ASSISTANT",
        };
        out.push_str(label);
        out.push_str(": ");
        out.push_str(turn.text.trim());
        out.push_str("\n\n");
    }
    out.push_str(
        "[End of priming. The user has not sent a new message yet — \
         acknowledge briefly that you have the context and stand by.]",
    );
    out
}

/// Resolve the operator's timezone for scheduling. Reads `NUCLEUS_TZ`,
/// then `TZ`, falling back to UTC if neither parses. In real deployments
/// `NUCLEUS_TZ` is always set (the launchd plist + dotenv populate it),
/// so the fallback only ever fires in tests.
pub fn nucleus_tz() -> chrono_tz::Tz {
    let candidates = [std::env::var("NUCLEUS_TZ").ok(), std::env::var("TZ").ok()];
    for c in candidates.iter().flatten() {
        if c.is_empty() {
            continue;
        }
        if let Ok(tz) = c.parse::<chrono_tz::Tz>() {
            return tz;
        }
    }
    chrono_tz::UTC
}

/// Sleep until the next 04:00 local time in [`nucleus_tz`]. Used by each
/// bot's main loop to gate the daily rotation tick. Returns immediately
/// after the wakeup; callers run the rotation, then call this again to
/// schedule the following day.
///
/// Testing override: setting `NUCLEUS_ROTATION_TEST_DELAY_SECONDS` to a
/// positive integer short-circuits the 4am math and sleeps that many
/// seconds instead. Lets us validate the rotation end-to-end on a live
/// bot without waiting until 4am. Leave unset in production.
pub async fn sleep_until_next_4am() {
    if let Ok(s) = std::env::var("NUCLEUS_ROTATION_TEST_DELAY_SECONDS") {
        if let Ok(secs) = s.parse::<u64>() {
            if secs > 0 {
                tokio::time::sleep(Duration::from_secs(secs)).await;
                return;
            }
        }
    }
    let delay = duration_until_next_4am(chrono::Utc::now(), nucleus_tz());
    tokio::time::sleep(delay).await;
}

/// Compute how long from `now_utc` until the next 04:00 in `tz`. Pulled
/// out so we can unit-test the wraparound (e.g. it's 03:30 → 30min; it's
/// 04:30 → 23h30m).
fn duration_until_next_4am(now_utc: chrono::DateTime<chrono::Utc>, tz: chrono_tz::Tz) -> Duration {
    use chrono::{Datelike, TimeZone};
    let now_local = now_utc.with_timezone(&tz);
    let today_4am = tz
        .with_ymd_and_hms(now_local.year(), now_local.month(), now_local.day(), 4, 0, 0)
        .single();
    let target_local = match today_4am {
        Some(t) if now_local < t => t,
        _ => {
            let tomorrow = now_local.date_naive() + chrono::Duration::days(1);
            tz.with_ymd_and_hms(tomorrow.year(), tomorrow.month(), tomorrow.day(), 4, 0, 0)
                .single()
                .unwrap_or_else(|| now_local + chrono::Duration::hours(24))
        }
    };
    let target_utc = target_local.with_timezone(&chrono::Utc);
    let delta = target_utc - now_utc;
    // Clamp to a sane lower bound — a negative delta would mean we
    // somehow computed a target in the past; fall back to a full day.
    if delta.num_seconds() <= 0 {
        Duration::from_secs(24 * 60 * 60)
    } else {
        Duration::from_secs(delta.num_seconds() as u64)
    }
}

/// Reduce arbitrary chat-id strings into safe tmux window names: lowercase
/// alphanumeric + dash, max 16 chars.
fn sanitize_window_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c.to_ascii_lowercase() } else { '-' })
        .collect();
    cleaned.chars().take(16).collect()
}

// ---- internals ----

fn build_claude_args(session_id: &str, resuming: bool, opts: &SpawnOptions) -> Vec<String> {
    let mut args: Vec<String> = if resuming {
        vec!["--resume".into(), session_id.into()]
    } else {
        vec!["--session-id".into(), session_id.into()]
    };
    if let Some(mode) = opts.permission_mode {
        args.push("--permission-mode".into());
        args.push(mode.as_arg().into());
    }
    if let Some(ref prompt) = opts.append_system_prompt {
        args.push("--append-system-prompt".into());
        args.push(prompt.clone());
    }
    for dir in &opts.add_dirs {
        args.push("--add-dir".into());
        args.push(dir.to_string_lossy().into_owned());
    }
    if !opts.disallowed_tools.is_empty() {
        args.push("--disallowed-tools".into());
        args.push(opts.disallowed_tools.join(" "));
    }
    if !opts.allowed_tools.is_empty() {
        args.push("--allowed-tools".into());
        args.push(opts.allowed_tools.join(" "));
    }
    args
}

fn transcript_path_for(workspace_root: &Path, session_id: &str) -> PathBuf {
    let encoded = workspace_root.to_string_lossy().replace('/', "-");
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home)
        .join(".claude")
        .join("projects")
        .join(encoded)
        .join(format!("{}.jsonl", session_id))
}

async fn ensure_tmux_session(name: &str) -> Result<()> {
    let has = Command::new("tmux")
        .args(["has-session", "-t", name])
        .output()
        .await?;
    if has.status.success() {
        return Ok(());
    }
    let out = Command::new("tmux")
        .args(["new-session", "-d", "-s", name])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!(
            "tmux new-session failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Send `content` into the target pane via paste-buffer (robust to quotes,
/// newlines, emoji, etc.) and then press Enter.
async fn paste_and_send(target: &str, content: &str) -> Result<()> {
    // Load buffer from stdin.
    let mut child = Command::new("tmux")
        .args(["load-buffer", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawning tmux load-buffer")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(content.as_bytes()).await?;
        stdin.shutdown().await?;
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        anyhow::bail!(
            "tmux load-buffer failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let p = Command::new("tmux")
        .args(["paste-buffer", "-t", target])
        .output()
        .await?;
    if !p.status.success() {
        anyhow::bail!(
            "tmux paste-buffer failed: {}",
            String::from_utf8_lossy(&p.stderr).trim()
        );
    }

    // Wait for the bracketed-paste to fully drain into claude's TUI before
    // pressing Enter. Without this, large pastes leave the TUI mid-paste-mode
    // when Enter arrives, so the Enter gets eaten as a literal newline and
    // the prompt sits queued unsent. Poll: pane is "settled" when consecutive
    // captures match for 250ms.
    wait_for_input_settled(target, Duration::from_millis(250), Duration::from_secs(10)).await?;

    let e = Command::new("tmux")
        .args(["send-keys", "-t", target, "Enter"])
        .output()
        .await?;
    if !e.status.success() {
        anyhow::bail!(
            "tmux send-keys Enter failed: {}",
            String::from_utf8_lossy(&e.stderr).trim()
        );
    }
    Ok(())
}

/// Poll the pane content; if claude's "trust this folder" prompt is showing,
/// send Enter (default answer = trust). Returns Ok whether the prompt was
/// found or not — the absence is the common case after first run.
async fn dismiss_trust_prompt_if_present(target: &str, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let out = Command::new("tmux")
            .args(["capture-pane", "-t", target, "-p"])
            .output()
            .await?;
        let pane = String::from_utf8_lossy(&out.stdout);
        if pane.contains("trust this folder") || pane.contains("trust this folder?") {
            // Default highlighted option is "Yes, I trust this folder".
            let _ = Command::new("tmux")
                .args(["send-keys", "-t", target, "Enter"])
                .output()
                .await;
            return Ok(());
        }
        // If the transcript-creation phase has started, the prompt didn't fire.
        if pane.contains("│") && pane.contains(">") && !pane.contains("trust") {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}

/// Poll the pane until consecutive captures stay identical for `settle_window`.
/// Used after `paste-buffer` to wait for the bracketed-paste sequence to fully
/// drain into the TUI before pressing Enter; otherwise Enter gets eaten inside
/// the paste and the prompt sits queued.
async fn wait_for_input_settled(
    target: &str,
    settle_window: Duration,
    deadline: Duration,
) -> Result<()> {
    let start = Instant::now();
    let mut last = String::new();
    let mut last_change = Instant::now();
    while start.elapsed() < deadline {
        let out = Command::new("tmux")
            .args(["capture-pane", "-t", target, "-p"])
            .output()
            .await?;
        let cur = String::from_utf8_lossy(&out.stdout).into_owned();
        if cur != last {
            last = cur;
            last_change = Instant::now();
        } else if last_change.elapsed() >= settle_window {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Best-effort: proceed even if we never hit a clean settle.
    Ok(())
}

/// Claude's TUI shows an input row (the "❯" caret) once it's done loading.
/// Poll for that marker before sending keys, otherwise the first message
/// can get eaten by the boot sequence.
///
/// Some pre-input screens look "ready" in the naive sense (they have the ❯
/// glyph) but actually want a numbered-option keypress before they yield
/// the real input prompt. The big one observed in the wild: large stored
/// sessions launch into a "Resume from summary?" picker on `--resume`.
/// Three orphan tmux windows accumulated during one chat-session outage
/// because the naive wait_for_tui_ready timed out on this picker without
/// ever detecting it. Now: detect the picker, auto-dismiss with option 1
/// (the default — "Resume from summary"), retry the readiness check. Any
/// other recognized-but-unhandled prompt bails with a descriptive name
/// so the caller can surface it instead of silently looping.
async fn wait_for_tui_ready(target: &str, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    let mut resume_dismiss_attempts: u8 = 0;
    const MAX_RESUME_DISMISSALS: u8 = 2;
    while start.elapsed() < timeout {
        let out = Command::new("tmux")
            .args(["capture-pane", "-t", target, "-p"])
            .output()
            .await?;
        let pane = String::from_utf8_lossy(&out.stdout);

        // Ready: input row + a marker that we're past boot. Status-line
        // text varies across versions ("auto mode on", "Try ..." hints).
        if pane.contains("❯") && (pane.contains("auto mode") || pane.contains("Try ")) {
            return Ok(());
        }

        // Resume-from-summary picker. Default option (1) is "Resume from
        // summary" — what we want for long-lived chat sessions. Send "1"
        // + Enter and let the next poll iteration see the real input row.
        if pane.contains("Resume from summary") {
            if resume_dismiss_attempts >= MAX_RESUME_DISMISSALS {
                anyhow::bail!(
                    "TUI blocked at interactive prompt: ResumeFromSummary (auto-dismiss failed after {} attempts)",
                    MAX_RESUME_DISMISSALS
                );
            }
            resume_dismiss_attempts += 1;
            let _ = Command::new("tmux")
                .args(["send-keys", "-t", target, "1"])
                .output()
                .await;
            let _ = Command::new("tmux")
                .args(["send-keys", "-t", target, "Enter"])
                .output()
                .await;
            // Settle delay before the next pane capture: dismissing
            // the picker takes a beat to re-render the input row.
            tokio::time::sleep(Duration::from_millis(300)).await;
            continue;
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!("TUI did not become ready within {:?}", timeout);
}

/// Poll the transcript file's size until it's been unchanged for
/// `settle_window`. Returns the byte length at that quiet moment, which
/// the caller uses as the `from_offset` for the next `wait_for_assistant`.
/// Returns `None` if the file is unreadable, so the caller can fall back
/// to its cached cursor. Bounded by an internal max-wait so a pathological
/// long-running emission can't block the call forever.
async fn wait_for_transcript_quiet(path: &Path, settle_window: Duration) -> Option<u64> {
    let mut last_size = tokio::fs::metadata(path).await.ok().map(|m| m.len())?;
    let mut last_change = Instant::now();
    let start = Instant::now();
    const MAX_WAIT: Duration = Duration::from_secs(60);
    loop {
        let size = tokio::fs::metadata(path).await.ok().map(|m| m.len())?;
        if size != last_size {
            last_size = size;
            last_change = Instant::now();
        } else if last_change.elapsed() >= settle_window {
            return Some(size);
        }
        if start.elapsed() >= MAX_WAIT {
            return Some(last_size);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Poll the transcript file from `from_offset` onward until we've seen at
/// least one assistant message AND no new bytes have arrived for
/// `quiescent_window`. Returns the concatenated text of the last assistant
/// message.
async fn wait_for_assistant(
    path: &Path,
    from_offset: u64,
    max_wait: Duration,
    quiescent_window: Duration,
) -> Result<String> {
    let start = Instant::now();
    let mut last_change = Instant::now();
    let mut last_size: u64 = from_offset;
    let mut buffer = String::new();
    let mut have_assistant = false;

    loop {
        if start.elapsed() > max_wait {
            anyhow::bail!("timed out after {:?} waiting for assistant response", max_wait);
        }

        // File is created lazily on first message; tolerate it being absent
        // for the first few hundred ms.
        let size = match tokio::fs::metadata(path).await {
            Ok(m) => m.len(),
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        if size > last_size {
            let mut file = tokio::fs::File::open(path).await?;
            file.seek(SeekFrom::Start(last_size)).await?;
            let mut new_bytes = String::new();
            file.read_to_string(&mut new_bytes).await?;
            buffer.push_str(&new_bytes);
            last_size = size;
            last_change = Instant::now();
            if !have_assistant {
                have_assistant = buffer.lines().any(|l| line_is_assistant(l));
            }
        }

        if have_assistant && last_change.elapsed() > quiescent_window {
            if let Some(text) = extract_last_assistant_text(&buffer) {
                return Ok(text);
            }
        }

        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[derive(Deserialize)]
struct EventEnvelope {
    #[serde(rename = "type")]
    kind: String,
    message: Option<serde_json::Value>,
}

fn line_is_assistant(line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() {
        return false;
    }
    serde_json::from_str::<EventEnvelope>(line)
        .map(|e| e.kind == "assistant")
        .unwrap_or(false)
}

fn extract_last_assistant_text(buffer: &str) -> Option<String> {
    let mut last: Option<String> = None;
    for line in buffer.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<EventEnvelope>(line) else {
            continue;
        };
        if event.kind != "assistant" {
            continue;
        }
        let Some(msg) = event.message else { continue };
        let Some(content) = msg.get("content").and_then(|c| c.as_array()) else {
            continue;
        };
        let mut text = String::new();
        for block in content {
            if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                    text.push_str(t);
                }
            }
        }
        let trimmed = text.trim().to_string();
        if !trimmed.is_empty() {
            last = Some(trimmed);
        }
    }
    last
}

/// Role of a transcript turn. Used by [`last_n_turns`] to label what came
/// from the user vs. the assistant for downstream prompt construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRole {
    User,
    Assistant,
}

/// A single user/assistant text turn extracted from a session transcript.
#[derive(Debug, Clone)]
pub struct Turn {
    pub role: TurnRole,
    pub text: String,
}

/// Read the last `n` user/assistant text turns from a session transcript
/// JSONL. Filters out non-conversational entries (tool_use, tool_result,
/// thinking blocks, file-history snapshots, etc.) and Claude Code's
/// system-injected user turns (`<ide_opened_file>`, `<system-reminder>`,
/// etc.) that aren't the operator's own words. Strips the
/// `[context: today is …]` date preamble we inject in [`with_date_preamble`]
/// so the replay reads as the user's original message.
///
/// Returns turns in chronological order — oldest first, newest last.
/// Returns an empty Vec if the transcript file is missing or unreadable;
/// callers should treat that as "nothing to replay" rather than an error.
pub fn last_n_turns(transcript_path: &Path, n: usize) -> Vec<Turn> {
    use std::io::BufRead;
    let file = match std::fs::File::open(transcript_path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let reader = std::io::BufReader::new(file);
    let mut turns: Vec<Turn> = Vec::new();
    for line in reader.lines().map_while(|r| r.ok()) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let kind = v.get("type").and_then(|s| s.as_str()).unwrap_or("");
        let role = match kind {
            "user" => TurnRole::User,
            "assistant" => TurnRole::Assistant,
            _ => continue,
        };
        let Some(content) = v.get("message").and_then(|m| m.get("content")) else {
            continue;
        };
        let mut text_parts: Vec<&str> = Vec::new();
        if let Some(s) = content.as_str() {
            text_parts.push(s);
        } else if let Some(arr) = content.as_array() {
            for item in arr {
                if item.get("type").and_then(|s| s.as_str()) != Some("text") {
                    continue;
                }
                if let Some(t) = item.get("text").and_then(|s| s.as_str()) {
                    text_parts.push(t);
                }
            }
        }
        if text_parts.is_empty() {
            continue;
        }
        let mut text = text_parts.join("\n");
        if matches!(role, TurnRole::User) && is_system_injected_user_turn(&text) {
            continue;
        }
        text = strip_date_preamble(&text).trim().to_string();
        if text.is_empty() {
            continue;
        }
        turns.push(Turn { role, text });
    }
    if turns.len() > n {
        let drop = turns.len() - n;
        turns.drain(..drop);
    }
    turns
}

/// Claude Code injects synthetic `<…>`-wrapped user turns (IDE state,
/// system reminders, slash-command echoes) into the transcript. They show
/// up as `role: user` but the operator never typed them; replaying them
/// would confuse a fresh session.
fn is_system_injected_user_turn(text: &str) -> bool {
    let t = text.trim_start();
    const PREFIXES: &[&str] = &[
        "<ide_opened_file>",
        "<ide_diagnostics>",
        "<system-reminder>",
        "<command-message>",
        "<command-name>",
        "<command-args>",
        "<local-command-",
    ];
    PREFIXES.iter().any(|p| t.starts_with(p))
}

fn strip_date_preamble(s: &str) -> &str {
    const TAG: &str = "[context: today is ";
    if let Some(rest) = s.strip_prefix(TAG) {
        if let Some(idx) = rest.find("]\n\n") {
            return &rest[idx + 3..];
        }
    }
    s
}

/// Single-quote shell escape: `it's` → `'it'\''s'`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Prepend a fresh wall-clock context line to every ask payload. Long-lived
/// `SessionPool` sessions otherwise stay anchored to spawn-day "today" — the
/// model has no internal clock and a single `date` lookup at session start
/// gets carried as the anchor across every subsequent turn. Recomputing per
/// ask() keeps "tomorrow" / "in N hours" reasoning honest. Falls back to a
/// UTC stamp if local timezone resolution fails.
fn with_date_preamble(message: &str) -> String {
    use chrono::Local;
    let now = Local::now();
    let stamp = now.format("%Y-%m-%d (%a), local %H:%M %Z").to_string();
    format!("[context: today is {stamp}]\n\n{message}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Set up a tmux session whose pane contains synthetic text resembling
    /// the resume-from-summary picker. wait_for_tui_ready should detect it,
    /// auto-dismiss with "1" + Enter, and then either find the ready marker
    /// (if we follow up with one) or time out. We don't need a real claude;
    /// we just need pane content that triggers the auto-dismiss code path
    /// and verify the keys land.
    async fn tmux_kill(session: &str) {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", session])
            .output()
            .await;
    }

    async fn pane_content(target: &str) -> String {
        let out = Command::new("tmux")
            .args(["capture-pane", "-t", target, "-p"])
            .output()
            .await
            .expect("tmux capture-pane");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Driver: synthesize a "Resume from summary" pane, then have a
    /// background task overwrite it with a ready-looking pane once we've
    /// observed the auto-dismiss keys arrive. Asserts the ready path
    /// completes within a generous timeout (well under the 60s spawn
    /// budget).
    #[tokio::test]
    async fn wait_for_tui_ready_auto_dismisses_resume_picker() {
        let session = "nucleus-tui-ready-test";
        tmux_kill(session).await;

        // tmux new-session pinning a noop shell so the pane is alive and
        // we can `respawn-window` text into it via printf.
        let out = Command::new("tmux")
            .args(["new-session", "-d", "-s", session, "cat"])
            .output()
            .await
            .expect("tmux new-session");
        assert!(out.status.success(), "tmux new-session failed");
        let target = format!("{session}:0");

        // Seed pane with the resume-picker text. We send via send-keys
        // so the bytes land in the pane buffer; the `cat` process keeps
        // them visible.
        let seed = "❯ Resume from summary?\n  1. Resume from summary\n  2. Start fresh\n";
        let _ = Command::new("tmux")
            .args(["send-keys", "-t", &target, seed])
            .output()
            .await;
        // Give tmux a moment to render before we start polling.
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Background task: once the pane shows the dismissal keystrokes
        // (the test pane will literally echo "1\n" because cat is the
        // process), rewrite the pane with a ready-looking screen.
        let target_clone = target.clone();
        let painter = tokio::spawn(async move {
            for _ in 0..40 {
                let pane = pane_content(&target_clone).await;
                if pane.contains("1\n") || pane.contains("\n1\n") {
                    // Replace pane content: clear, then write a ready frame.
                    let _ = Command::new("tmux")
                        .args(["send-keys", "-t", &target_clone, "C-l"])
                        .output()
                        .await;
                    let _ = Command::new("tmux")
                        .args([
                            "send-keys",
                            "-t",
                            &target_clone,
                            "❯ ready\nTry asking me something\nauto mode on\n",
                        ])
                        .output()
                        .await;
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });

        let res = wait_for_tui_ready(&target, Duration::from_secs(8)).await;
        let _ = painter.await;
        tmux_kill(session).await;

        assert!(res.is_ok(), "wait_for_tui_ready failed: {:?}", res);
    }

    /// When wait_for_tui_ready is pointed at a pane that's stuck on a
    /// prompt it doesn't know how to dismiss (here: a plausible-looking
    /// "Choose the credential" auth picker we haven't taught it about),
    /// it should still time out — but only after the timeout expires.
    /// We don't test the *named* prompt path here (we haven't added one
    /// beyond ResumeFromSummary); this just asserts the timeout path
    /// still bails as before.
    #[tokio::test]
    async fn wait_for_tui_ready_times_out_on_unknown_prompt() {
        let session = "nucleus-tui-ready-test-2";
        tmux_kill(session).await;
        let out = Command::new("tmux")
            .args(["new-session", "-d", "-s", session, "cat"])
            .output()
            .await
            .expect("tmux new-session");
        assert!(out.status.success());
        let target = format!("{session}:0");
        let _ = Command::new("tmux")
            .args([
                "send-keys",
                "-t",
                &target,
                "Choose the credential to use:\n  1. account-a\n  2. account-b\n",
            ])
            .output()
            .await;
        let res = wait_for_tui_ready(&target, Duration::from_millis(800)).await;
        tmux_kill(session).await;
        assert!(res.is_err(), "expected timeout, got: {:?}", res);
        let msg = res.unwrap_err().to_string();
        assert!(
            msg.contains("did not become ready"),
            "unexpected error message: {msg}"
        );
    }

    /// Build a transcript JSONL with a realistic mix of entries — user
    /// text, assistant text, tool_use, tool_result, system-injected user
    /// turns, and a date-preamble-wrapped user message — then assert
    /// last_n_turns returns only the operator-meaningful text turns,
    /// chronologically, capped at N.
    #[test]
    fn last_n_turns_filters_and_orders_correctly() {
        let tmp = std::env::temp_dir().join(format!(
            "nucleus-last-n-turns-{}.jsonl",
            std::process::id()
        ));
        let mut lines: Vec<String> = Vec::new();
        // Non-conversational entries that should be ignored entirely.
        lines.push(r#"{"type":"permission-mode","permissionMode":"auto"}"#.to_string());
        lines.push(r#"{"type":"file-history-snapshot","messageId":"abc"}"#.to_string());
        // System-injected user turn — must be skipped.
        lines.push(r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"<ide_opened_file>some log</ide_opened_file>"}]}}"#.to_string());
        // Date-preamble wrapped real user message — preamble stripped.
        lines.push(r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[context: today is 2026-05-23 (Sat), local 09:00 BRT]\n\nhello there"}]}}"#.to_string());
        // Assistant thinking + tool_use — ignored (no text block).
        lines.push(r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"…"},{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#.to_string());
        // Tool result entries are tagged role:user — must be skipped.
        lines.push(r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#.to_string());
        // Assistant text reply.
        lines.push(r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi! how can I help?"}]}}"#.to_string());
        // Another user message (string-form content rather than array form).
        lines.push(r#"{"type":"user","message":{"role":"user","content":"second user message"}}"#.to_string());
        // Assistant reply combining thinking + text — text block kept.
        lines.push(r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"…"},{"type":"text","text":"second assistant reply"}]}}"#.to_string());
        std::fs::write(&tmp, lines.join("\n")).expect("write tmp jsonl");

        let turns = last_n_turns(&tmp, 10);
        let _ = std::fs::remove_file(&tmp);

        assert_eq!(turns.len(), 4, "expected 4 turns, got {:?}", turns);
        assert!(matches!(turns[0].role, TurnRole::User));
        assert_eq!(turns[0].text, "hello there");
        assert!(matches!(turns[1].role, TurnRole::Assistant));
        assert_eq!(turns[1].text, "hi! how can I help?");
        assert!(matches!(turns[2].role, TurnRole::User));
        assert_eq!(turns[2].text, "second user message");
        assert!(matches!(turns[3].role, TurnRole::Assistant));
        assert_eq!(turns[3].text, "second assistant reply");
    }

    /// When the transcript has more than N text turns, we keep only the
    /// last N and drop the older ones.
    #[test]
    fn last_n_turns_caps_at_n() {
        let tmp = std::env::temp_dir().join(format!(
            "nucleus-last-n-turns-cap-{}.jsonl",
            std::process::id()
        ));
        let mut lines = Vec::new();
        for i in 0..15 {
            lines.push(format!(
                r#"{{"type":"user","message":{{"role":"user","content":"u{i}"}}}}"#
            ));
            lines.push(format!(
                r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"a{i}"}}]}}}}"#
            ));
        }
        std::fs::write(&tmp, lines.join("\n")).expect("write tmp jsonl");

        let turns = last_n_turns(&tmp, 10);
        let _ = std::fs::remove_file(&tmp);

        assert_eq!(turns.len(), 10);
        // Last 10 of 30 turns = u10..a14 (alternating).
        assert_eq!(turns[0].text, "u10");
        assert_eq!(turns[9].text, "a14");
    }

    /// Missing transcript file returns an empty Vec — callers treat this
    /// as "nothing to replay" without crashing the rotation.
    #[test]
    fn last_n_turns_missing_file_returns_empty() {
        let tmp = std::env::temp_dir().join("nucleus-last-n-turns-nope.jsonl");
        let _ = std::fs::remove_file(&tmp);
        let turns = last_n_turns(&tmp, 10);
        assert!(turns.is_empty());
    }

    /// Priming preamble structural sanity: summary up top, alternating
    /// USER:/ASSISTANT: lines, closing instruction at the bottom.
    #[test]
    fn build_priming_preamble_orders_summary_then_replay() {
        let replay = vec![
            Turn {
                role: TurnRole::User,
                text: "hello".into(),
            },
            Turn {
                role: TurnRole::Assistant,
                text: "hi there".into(),
            },
        ];
        let out = build_priming_preamble("- did X\n- decided Y", &replay);
        let summary_idx = out.find("- did X").expect("summary present");
        let user_idx = out.find("USER: hello").expect("user line present");
        let asst_idx = out.find("ASSISTANT: hi there").expect("assistant line present");
        let closing_idx = out.find("End of priming").expect("closing line present");
        assert!(summary_idx < user_idx);
        assert!(user_idx < asst_idx);
        assert!(asst_idx < closing_idx);
    }

    /// Wraparound: at 03:30 local we sleep ~30 minutes; at 04:30 local we
    /// sleep ~23h30m. Use UTC as the timezone so the test doesn't depend
    /// on chrono-tz database availability for a specific region.
    #[test]
    fn duration_until_next_4am_wraps_correctly() {
        use chrono::{TimeZone, Utc};
        let tz = chrono_tz::UTC;

        // 03:30 UTC → next 04:00 UTC is 30 minutes away.
        let at_0330 = Utc.with_ymd_and_hms(2026, 5, 23, 3, 30, 0).unwrap();
        let d = duration_until_next_4am(at_0330, tz);
        assert_eq!(d.as_secs(), 30 * 60);

        // 04:30 UTC → next 04:00 is tomorrow, 23h30m away.
        let at_0430 = Utc.with_ymd_and_hms(2026, 5, 23, 4, 30, 0).unwrap();
        let d = duration_until_next_4am(at_0430, tz);
        assert_eq!(d.as_secs(), 23 * 3600 + 30 * 60);

        // Exactly 04:00 → next 04:00 is tomorrow (24h), not "right now".
        let at_0400 = Utc.with_ymd_and_hms(2026, 5, 23, 4, 0, 0).unwrap();
        let d = duration_until_next_4am(at_0400, tz);
        assert_eq!(d.as_secs(), 24 * 3600);
    }

    /// nucleus_tz honors NUCLEUS_TZ when set, falls back to UTC otherwise.
    /// Saves/restores the env var so test order doesn't matter.
    #[test]
    fn nucleus_tz_reads_env() {
        let saved_tz = std::env::var("NUCLEUS_TZ").ok();
        let saved_posix = std::env::var("TZ").ok();
        // SAFETY: tests run single-threaded by default for env mutation;
        // we restore at the end. The known-good IANA name here is just
        // for the env round-trip — chrono_tz parses any valid IANA id.
        unsafe {
            std::env::set_var("NUCLEUS_TZ", "Europe/Berlin");
            std::env::remove_var("TZ");
        }
        let tz = nucleus_tz();
        assert_eq!(tz, chrono_tz::Europe::Berlin);

        unsafe {
            std::env::remove_var("NUCLEUS_TZ");
        }
        let tz = nucleus_tz();
        assert_eq!(tz, chrono_tz::UTC);

        // Restore.
        unsafe {
            match saved_tz {
                Some(v) => std::env::set_var("NUCLEUS_TZ", v),
                None => std::env::remove_var("NUCLEUS_TZ"),
            }
            match saved_posix {
                Some(v) => std::env::set_var("TZ", v),
                None => std::env::remove_var("TZ"),
            }
        }
    }
}
