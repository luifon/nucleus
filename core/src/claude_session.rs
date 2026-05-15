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
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            workspace_root: PathBuf::from("."),
            append_system_prompt: None,
            permission_mode: None,
            disallowed_tools: vec![],
            add_dirs: vec![],
            tmux_session: "nucleus".into(),
            window_name: None,
            ready_timeout: Duration::from_secs(20),
            resume_session_id: None,
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
        dismiss_trust_prompt_if_present(&target, Duration::from_secs(5)).await?;
        // Claude only creates the transcript file when it gets its first
        // message. Wait instead for the TUI to render the input prompt — at
        // that point send-keys is safe.
        wait_for_tui_ready(&target, opts.ready_timeout).await?;
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

        Ok(Self {
            session_id,
            tmux_target: target,
            transcript_path,
            cursor,
        })
    }

    /// Send a user message and wait for claude's next assistant reply. Blocks
    /// for at most `opts.max_wait`.
    pub async fn ask(&mut self, message: &str, opts: AskOptions) -> Result<String> {
        let from = self.cursor;
        paste_and_send(&self.tmux_target, message).await?;
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
    pub async fn close(self) -> Result<()> {
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
    pub add_dirs: Vec<PathBuf>,
    pub tmux_session: String,
    /// Sessions idle for longer than this get reaped on the next reap_idle()
    /// call. Set generously — re-spawning costs ~5s.
    pub idle_timeout: Duration,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            workspace_root: PathBuf::from("."),
            append_system_prompt: None,
            permission_mode: None,
            disallowed_tools: vec![],
            add_dirs: vec![],
            tmux_session: "nucleus".into(),
            idle_timeout: Duration::from_secs(60 * 60 * 4), // 4h
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
                    add_dirs: self.config.add_dirs.clone(),
                    tmux_session: self.config.tmux_session.clone(),
                    window_name: Some(window_name),
                    ready_timeout: Duration::from_secs(20),
                    resume_session_id,
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
async fn wait_for_tui_ready(target: &str, timeout: Duration) -> Result<()> {
    let start = Instant::now();
    while start.elapsed() < timeout {
        let out = Command::new("tmux")
            .args(["capture-pane", "-t", target, "-p"])
            .output()
            .await?;
        let pane = String::from_utf8_lossy(&out.stdout);
        // The input prompt and "auto mode on" / similar status line both
        // indicate claude is past boot and ready to accept input.
        if pane.contains("❯") && (pane.contains("auto mode") || pane.contains("Try ")) {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    anyhow::bail!("TUI did not become ready within {:?}", timeout);
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

/// Single-quote shell escape: `it's` → `'it'\''s'`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}
