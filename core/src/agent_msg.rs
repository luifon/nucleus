//! Agent-to-agent session messaging (ADR-021).
//!
//! The ONE sanctioned way an agent writes into another agent's live Claude
//! session. Raw `tmux send-keys` incantations are folklore; this module is
//! policy: registry-gated targets, an idle gate, a machine-written
//! attribution header with a hop limit, verified submit (the 2026-07-18
//! wedge class), and a durable injection log.
//!
//! Security model (ADR-021):
//! - Injection changes who may ASK, never what the target may DO — the
//!   receiving session's permission posture applies to injected turns
//!   exactly as to operator turns.
//! - Consent does not travel over injection: a sender must never assert
//!   operator authorization. Receiving personas treat `[agent-msg]` turns
//!   as untrusted peer input.

use crate::agents::Registry;
use crate::claude_session::paste_and_submit_verified;
use anyhow::{Context, Result, bail};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::process::Command;

/// Maximum hop a message may carry. `hop:1` is terminal: a session acting on
/// an injected turn must not inject onward (two sessions politely
/// instructing each other forever is the failure mode).
pub const MAX_HOP: u8 = 1;

pub struct SendOpts {
    /// Target tmux `session[:window]`. The session part must belong to a
    /// registered agent (exact match or `<registered>-suffix`, e.g. the
    /// per-chat pools under `nucleus-whatsapp-*`).
    pub to: String,
    /// Sender's agent label — recorded in the header and the log.
    pub from: String,
    /// The message body. The attribution header is prepended by THIS module;
    /// senders cannot supply or forge it.
    pub message: String,
    /// Hop count of the agent-msg this send is reacting to (0 = originating).
    pub hop: u8,
    /// Wait for the target's reply (transcript-tailed) up to this long.
    pub await_reply: Option<Duration>,
    pub workspace_root: PathBuf,
}

#[derive(Debug)]
pub struct SendReport {
    pub target: String,
    pub header: String,
    /// Present when `await_reply` was set and a reply arrived in time.
    pub reply: Option<String>,
}

/// Compose the machine-written attribution header.
fn header(from: &str, at: &str, hop: u8) -> String {
    format!("[agent-msg from:{from} at:{at} hop:{hop}]")
}

/// Validate `to` against the registry: the session part must be a registered
/// agent's tmux_session, exactly or as prefix (`nucleus-whatsapp` covers
/// `nucleus-whatsapp-dm:1` — the /agents convention).
fn validate_target(registry: &Registry, to: &str) -> Result<()> {
    let session = to.split(':').next().unwrap_or(to);
    let known = registry.agents.iter().filter_map(|a| a.tmux_session.as_deref());
    for s in known {
        if session == s || session.starts_with(&format!("{s}-")) {
            return Ok(());
        }
    }
    bail!(
        "target session {session:?} is not owned by any registered agent (agents.toml) — \
         refusing to inject into an unknown session"
    )
}

/// Idle gate: the target must show an EMPTY input row and no option picker.
/// Injecting into a mid-turn session races the TUI; injecting into a picker
/// would answer it. Bounded wait, then refuse.
async fn wait_for_idle(target: &str, deadline: Duration) -> Result<()> {
    let start = Instant::now();
    loop {
        let out = Command::new("tmux")
            .args(["capture-pane", "-t", target, "-p"])
            .output()
            .await
            .context("tmux capture-pane (does the target window exist?)")?;
        if out.status.success() {
            let pane = String::from_utf8_lossy(&out.stdout);
            // ONLY the LAST ❯ row is the live input — submitted messages
            // re-render in the scrollback with a ❯ prefix too, so scanning
            // every row mistakes history for a sitting draft (E2E, 2026-07-18).
            match crate::claude_session::last_prompt_row(&pane) {
                Some(rest) => {
                    // `Try "…"` is the fresh-TUI placeholder, not a draft —
                    // the TS wrapper's waitForTuiReady treats it as READY.
                    if rest.is_empty() || rest.starts_with("Try ") {
                        return Ok(());
                    }
                    if rest.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                        // "❯ 1. …" — an option picker / permission dialog.
                        tracing::warn!(target, "target is showing an interactive picker — waiting");
                    }
                }
                None => {} // no prompt row: booting or mid-turn — keep waiting
            }
        }
        if start.elapsed() >= deadline {
            bail!(
                "target {target} did not become idle within {}s — refusing to inject \
                 into a busy or wedged session",
                deadline.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn log_db(workspace_root: &Path) -> Result<SqlitePool> {
    let pool = crate::db::open(&workspace_root.join("memory/agent_messages.db")).await?;
    crate::migrate::migrate(
        &pool,
        &[crate::migrate::Migration {
            version: 1,
            name: "baseline agent_messages",
            step: crate::migrate::Step::Sql(
                "CREATE TABLE IF NOT EXISTS agent_messages (\n\
                   id        INTEGER PRIMARY KEY AUTOINCREMENT,\n\
                   at        TEXT    NOT NULL,\n\
                   sender    TEXT    NOT NULL,\n\
                   target    TEXT    NOT NULL,\n\
                   hop       INTEGER NOT NULL,\n\
                   preview   TEXT    NOT NULL,\n\
                   delivered INTEGER NOT NULL,\n\
                   error     TEXT\n\
                 );\n\
                 CREATE INDEX IF NOT EXISTS idx_agent_messages_at ON agent_messages(at DESC)",
            ),
        }],
    )
    .await?;
    Ok(pool)
}

async fn record(
    pool: &SqlitePool,
    at: &str,
    sender: &str,
    target: &str,
    hop: u8,
    preview: &str,
    delivered: bool,
    error: Option<&str>,
) {
    let _ = sqlx::query(
        "INSERT INTO agent_messages (at, sender, target, hop, preview, delivered, error)\n\
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(at)
    .bind(sender)
    .bind(target)
    .bind(hop as i64)
    .bind(preview)
    .bind(delivered)
    .bind(error)
    .execute(pool)
    .await;
}

/// Send an agent message into a registered live session (ADR-021).
pub async fn send(opts: SendOpts) -> Result<SendReport> {
    if opts.hop >= MAX_HOP {
        bail!(
            "hop limit: this send reacts to an agent-msg with hop:{} — hop:{MAX_HOP} is \
             terminal (ADR-021); a session acting on an injected turn must not inject onward",
            opts.hop
        );
    }
    if opts.from.trim().is_empty() {
        bail!("--from is required (attribution is mandatory, ADR-021)");
    }

    let registry = Registry::load_from(opts.workspace_root.join("agents.toml"))
        .context("loading agents.toml registry")?;
    validate_target(&registry, &opts.to)?;

    let at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let hdr = header(&opts.from, &at, opts.hop + 1);
    let payload = format!("{hdr}\n{}", opts.message);
    let preview: String = opts.message.chars().take(160).collect();

    let pool = log_db(&opts.workspace_root).await?;

    wait_for_idle(&opts.to, Duration::from_secs(30)).await?;

    // Snapshot transcript dir state BEFORE the send so await_reply can find
    // which transcript recorded our header.
    let send_started = std::time::SystemTime::now();

    if let Err(e) = paste_and_submit_verified(&opts.to, &payload).await {
        record(
            &pool,
            &at,
            &opts.from,
            &opts.to,
            opts.hop + 1,
            &preview,
            false,
            Some(&format!("{e:#}")),
        )
        .await;
        return Err(e);
    }
    record(&pool, &at, &opts.from, &opts.to, opts.hop + 1, &preview, true, None).await;

    let reply = match opts.await_reply {
        None => None,
        Some(timeout) => {
            Some(await_reply(&opts.workspace_root, &hdr, send_started, timeout).await?)
        }
    };

    Ok(SendReport { target: opts.to, header: hdr, reply })
}

/// Find the transcript that recorded our injected turn (it contains the
/// unique attribution header) and tail it for the turn's final assistant
/// text. v1 of the ADR-021 reply channel: transcript poll, no new IPC.
async fn await_reply(
    workspace_root: &Path,
    hdr: &str,
    since: std::time::SystemTime,
    timeout: Duration,
) -> Result<String> {
    let dir = transcripts_dir(workspace_root);
    let start = Instant::now();
    let mut transcript: Option<PathBuf> = None;

    // Phase 1: locate the transcript containing our header.
    while transcript.is_none() {
        if start.elapsed() >= timeout {
            bail!("reply timeout: no transcript recorded the injected message");
        }
        let mut candidates = Vec::new();
        if let Ok(mut rd) = tokio::fs::read_dir(&dir).await {
            while let Ok(Some(ent)) = rd.next_entry().await {
                let p = ent.path();
                if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                if let Ok(meta) = ent.metadata().await {
                    if let Ok(modified) = meta.modified() {
                        // Small grace window: mtimes are coarse.
                        if modified >= since - Duration::from_secs(5) {
                            candidates.push(p);
                        }
                    }
                }
            }
        }
        for p in candidates {
            if let Ok(text) = tokio::fs::read_to_string(&p).await {
                if text.contains(hdr) {
                    transcript = Some(p);
                    break;
                }
            }
        }
        if transcript.is_none() {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    let path = transcript.unwrap();

    // Phase 2: wait for the assistant turn AFTER our header to complete.
    // Completion = an assistant text block exists past the header and the
    // file has been quiet for 3s (late tool outputs settle), or end_turn.
    let mut last_len = 0u64;
    let mut quiet_since = Instant::now();
    loop {
        if start.elapsed() >= timeout {
            bail!("reply timeout: target session did not finish a reply in time");
        }
        let text = tokio::fs::read_to_string(&path).await.unwrap_or_default();
        let len = text.len() as u64;
        if len != last_len {
            last_len = len;
            quiet_since = Instant::now();
        }
        if let Some(idx) = text.find(hdr) {
            let after = &text[idx..];
            let mut latest: Option<String> = None;
            for line in after.lines() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                    if v.get("type").and_then(|t| t.as_str()) == Some("assistant") {
                        if let Some(content) =
                            v.pointer("/message/content").and_then(|c| c.as_array())
                        {
                            let mut txt = String::new();
                            for block in content {
                                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                                    if let Some(s) = block.get("text").and_then(|t| t.as_str()) {
                                        if !txt.is_empty() {
                                            txt.push('\n');
                                        }
                                        txt.push_str(s);
                                    }
                                }
                            }
                            if !txt.trim().is_empty() {
                                latest = Some(txt);
                            }
                        }
                    }
                }
            }
            if let Some(reply) = latest {
                if quiet_since.elapsed() >= Duration::from_secs(3) {
                    return Ok(reply);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
    }
}

fn transcripts_dir(workspace_root: &Path) -> PathBuf {
    let encoded = workspace_root.to_string_lossy().replace('/', "-");
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".claude").join("projects").join(encoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_is_machine_formatted() {
        assert_eq!(
            header("main", "2026-07-18T20:00:00Z", 1),
            "[agent-msg from:main at:2026-07-18T20:00:00Z hop:1]"
        );
    }

    #[tokio::test]
    async fn hop_limit_is_terminal() {
        let opts = SendOpts {
            to: "nucleus-whatsapp-dm:1".into(),
            from: "whatsapp".into(),
            message: "onward".into(),
            hop: 1,
            await_reply: None,
            workspace_root: PathBuf::from("/tmp"),
        };
        let err = send(opts).await.unwrap_err();
        assert!(format!("{err:#}").contains("hop limit"), "{err:#}");
    }

    #[test]
    fn registry_gate_prefix_matches_pool_sessions() {
        let dir = std::env::temp_dir().join(format!("agentmsg-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let toml = dir.join("agents.toml");
        std::fs::write(
            &toml,
            r#"
[[agent]]
name = "whatsapp"
class = "conversational"
launch = "launchd-daemon"
launchd_label = "dev.nucleus.whatsapp"
tmux_session = "nucleus-whatsapp"
"#,
        )
        .unwrap();
        let reg = Registry::load_from(&toml).unwrap();
        assert!(validate_target(&reg, "nucleus-whatsapp").is_ok());
        assert!(validate_target(&reg, "nucleus-whatsapp-dm:1").is_ok());
        assert!(validate_target(&reg, "nucleus-whatsapp-braindump").is_ok());
        assert!(validate_target(&reg, "nucleus-whatsappdm").is_err());
        assert!(validate_target(&reg, "some-random-session").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
