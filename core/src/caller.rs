//! Who is calling a Nucleus operator command (ADR-033, ADR-021).
//!
//! `nucleus tasks` and `nucleus session-send` run from several kinds of
//! caller, and they may not do the same things. The caller is decided from
//! the process tree ([`crate::proc_tree`]), never from the calling command's
//! own environment, which the command can change:
//!
//! - **The operator**: a terminal, or a Claude Code session the operator
//!   started. Every task, every command.
//! - **A WhatsApp DM chat session.** The bot starts each DM chat session's
//!   `claude` with `NUCLEUS_TASK_SCOPE=<token>` and records
//!   `sha256(token) → chat id` in whatsapp.db's `task_scopes` (one token per
//!   chat, revoked when the session ends). With a valid token the session may
//!   start tasks for that chat and read or cancel only the tasks that chat
//!   started. The origin comes from the token, never from a command-line
//!   argument. A chat session without a valid token (the group session, a
//!   session whose token was revoked) is refused.
//! - **A background task worker** (`NUCLEUS_SESSION=worker`). Workers may not
//!   start, cancel or read tasks and may not send agent messages.
//! - **Any other Nucleus session** (reminder fires, the Discord bot, the
//!   distiller, WhatsApp jobs). It may send agent messages as its own agent;
//!   it may not use background tasks.
//! - **A detached process** (no `claude` ancestor, no terminal): only the
//!   harmless `tasks sweep`. **An unreadable process tree**: nothing.
//!
//! `CLAUDE_CODE_SESSION_ID` in the caller's environment can only restrict: a
//! session id recorded in tasks.db is a worker and one recorded in
//! whatsapp.db's `chat_sessions` is a chat session, even for a process the
//! tree places at the operator.
//!
//! **Hop.** The session id (from the `claude` process's arguments, or the
//! environment for an operator session) locates the caller's transcript.
//! When the current turn read an attributed agent message
//! (`[agent-msg … hop:N]`), the caller is reacting to another agent, and
//! [`Caller::inbound_hop`] is `N`. `session-send` and `tasks start|cancel`
//! refuse such a turn: a session must not act on another agent's message by
//! messaging onward or starting work.

use crate::proc_tree::{self, Origin};
use crate::tasks::TASKS_DB_PATH;
use crate::whatsapp_queue::WHATSAPP_DB_PATH;
use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::Path;

/// Set by the WhatsApp bot for its DM chat sessions (a random token).
pub const ENV_TASK_SCOPE: &str = "NUCLEUS_TASK_SCOPE";
/// Set by the task worker for its session (the task id).
pub const ENV_TASK_WORKER: &str = "NUCLEUS_TASK_WORKER";
/// Set by Claude Code for every tool command a session runs.
pub const ENV_CLAUDE_SESSION: &str = "CLAUDE_CODE_SESSION_ID";

/// Only the end of a transcript is read for the hop check.
const TRANSCRIPT_TAIL_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// A terminal or a Claude Code session the operator started.
    Operator,
    /// A WhatsApp chat session with a valid task scope.
    Chat { origin: String, chat: String },
    /// A WhatsApp chat session without a valid task scope.
    UnscopedChat,
    Worker { task_id: Option<String> },
    /// Any other Nucleus session.
    Session { kind: String },
    /// No claude ancestor and no terminal.
    Detached,
    /// The process tree could not be read well enough to decide.
    Unknown(String),
}

#[derive(Debug, Clone)]
pub struct Caller {
    pub role: Role,
    /// The registry agent a Nucleus session runs as, from the environment
    /// its `claude` started with. `None` for the operator.
    pub agent: Option<String>,
    pub session_id: Option<String>,
    /// Highest `hop:` of an agent message the current turn read; 0 when the
    /// turn read none or no transcript was found.
    pub inbound_hop: u8,
}

impl Caller {
    /// The operator, for tests and in-process callers.
    pub fn operator() -> Self {
        Caller { role: Role::Operator, agent: None, session_id: None, inbound_hop: 0 }
    }

    /// A Nucleus session of any kind (chat, worker, other).
    pub fn is_nucleus_session(&self) -> bool {
        matches!(self.role, Role::Chat { .. } | Role::UnscopedChat | Role::Worker { .. } | Role::Session { .. })
    }
}

/// Hex SHA-256 of a scope token (what whatsapp.db stores).
pub fn scope_token_hash(token: &str) -> String {
    let d = Sha256::digest(token.as_bytes());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// Detect the caller of this process.
pub async fn detect(workspace_root: &Path) -> Result<Caller> {
    let origin = tokio::task::spawn_blocking(proc_tree::origin).await?;
    Ok(caller_for(workspace_root, &origin, env_nonempty(ENV_CLAUDE_SESSION).as_deref()).await)
}

/// [`detect`] for a given origin and environment session id.
pub(crate) async fn caller_for(workspace_root: &Path, origin: &Origin, env_session: Option<&str>) -> Caller {
    let session_id = match origin {
        Origin::Nucleus(n) => n.session_id.clone().or_else(|| env_session.map(str::to_string)),
        Origin::OperatorSession { session_id } => session_id.clone().or_else(|| env_session.map(str::to_string)),
        _ => env_session.map(str::to_string),
    };
    let inbound_hop = match &session_id {
        Some(id) => {
            let path = crate::claude_session::transcript_path_for(workspace_root, id);
            match tokio::fs::metadata(&path).await {
                Ok(_) => current_turn_hop(&path).await,
                Err(_) => 0,
            }
        }
        None => 0,
    };
    let agent = match origin {
        Origin::Nucleus(n) => n.agent.clone(),
        _ => None,
    };
    let role = role_for(workspace_root, origin, env_session).await;
    Caller { role, agent, session_id, inbound_hop }
}

async fn role_for(workspace_root: &Path, origin: &Origin, env_session: Option<&str>) -> Role {
    // Restrictive signals from the caller's own environment first: they can
    // only narrow what the tree allows.
    if let Some(task) = env_nonempty(ENV_TASK_WORKER) {
        return Role::Worker { task_id: Some(task) };
    }
    if let Some(sid) = env_session {
        if session_is_worker(workspace_root, sid).await {
            return Role::Worker { task_id: None };
        }
    }
    match origin {
        Origin::Unknown(reason) => Role::Unknown(reason.clone()),
        Origin::Detached => Role::Detached,
        Origin::Terminal | Origin::OperatorSession { .. } => {
            if let Some(sid) = env_session {
                if session_is_chat(workspace_root, sid).await {
                    return Role::UnscopedChat;
                }
            }
            Role::Operator
        }
        Origin::Nucleus(n) if n.is_worker() => Role::Worker { task_id: n.worker.clone() },
        Origin::Nucleus(n) if n.kind == proc_tree::SESSION_CHAT => match &n.scope {
            Some(token) => match scope_chat(workspace_root, token).await {
                Some(chat) => Role::Chat { origin: "whatsapp-dm".into(), chat },
                None => Role::UnscopedChat,
            },
            None => Role::UnscopedChat,
        },
        Origin::Nucleus(n) => Role::Session { kind: n.kind.clone() },
    }
}

async fn session_is_worker(workspace_root: &Path, sid: &str) -> bool {
    let path = workspace_root.join(TASKS_DB_PATH);
    if !path.exists() {
        return false;
    }
    let Ok(pool) = crate::db::open_read_only(&path).await else { return false };
    let hit: Option<String> = sqlx::query_scalar("SELECT id FROM tasks WHERE session_id = ?1 LIMIT 1")
        .bind(sid)
        .fetch_optional(&pool)
        .await
        .ok()
        .flatten();
    pool.close().await;
    hit.is_some()
}

async fn session_is_chat(workspace_root: &Path, sid: &str) -> bool {
    let path = workspace_root.join(WHATSAPP_DB_PATH);
    if !path.exists() {
        return false;
    }
    let Ok(pool) = crate::db::open_read_only(&path).await else { return false };
    let hit: Option<String> =
        sqlx::query_scalar("SELECT chat_id FROM chat_sessions WHERE session_id = ?1 LIMIT 1")
            .bind(sid)
            .fetch_optional(&pool)
            .await
            .ok()
            .flatten();
    pool.close().await;
    hit.is_some()
}

async fn scope_chat(workspace_root: &Path, token: &str) -> Option<String> {
    let path = workspace_root.join(WHATSAPP_DB_PATH);
    let pool = crate::db::open_read_only(&path).await.ok()?;
    let chat: Option<String> =
        sqlx::query_scalar("SELECT chat_id FROM task_scopes WHERE token_sha256 = ?1")
            .bind(scope_token_hash(token))
            .fetch_optional(&pool)
            .await
            .ok()
            .flatten();
    pool.close().await;
    chat
}

/// Highest agent-message hop among the inputs of the current turn of the
/// transcript at `path`: the records after the last `turn_duration`.
async fn current_turn_hop(path: &Path) -> u8 {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let Ok(mut f) = tokio::fs::File::open(path).await else { return 0 };
    let len = f.metadata().await.map(|m| m.len()).unwrap_or(0);
    let from = len.saturating_sub(TRANSCRIPT_TAIL_BYTES);
    if f.seek(std::io::SeekFrom::Start(from)).await.is_err() {
        return 0;
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).await.is_err() {
        return 0;
    }
    current_turn_hop_of(&String::from_utf8_lossy(&buf))
}

/// Pure core of [`current_turn_hop`].
pub fn current_turn_hop_of(transcript: &str) -> u8 {
    let records: Vec<serde_json::Value> = transcript
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect();
    let start = records
        .iter()
        .rposition(|v| v["type"] == "system" && v["subtype"] == "turn_duration")
        .map(|i| i + 1)
        .unwrap_or(0);
    let mut hop = 0u8;
    for v in &records[start..] {
        let text: Option<String> = match v["type"].as_str() {
            Some("user") if v["isMeta"] != true => match &v["message"]["content"] {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Array(items) => {
                    if items.iter().any(|b| b["type"] == "tool_result") {
                        None
                    } else {
                        Some(
                            items
                                .iter()
                                .filter_map(|b| b["text"].as_str())
                                .collect::<Vec<_>>()
                                .join("\n"),
                        )
                    }
                }
                _ => None,
            },
            Some("attachment") if v["attachment"]["type"] == "queued_command" => {
                v["attachment"]["prompt"].as_str().map(str::to_string)
            }
            _ => None,
        };
        if let Some(t) = text {
            hop = hop.max(agent_msg_hop(&t).unwrap_or(0));
        }
    }
    hop
}

/// The `hop:` of an `[agent-msg from:… at:… hop:N]` header line in `text`.
/// Only a header at the start of a line counts.
pub fn agent_msg_hop(text: &str) -> Option<u8> {
    let re = regex::Regex::new(r"(?m)^\[agent-msg from:\S+ at:\S+ hop:(\d+)\]").ok()?;
    re.captures_iter(text)
        .filter_map(|c| c[1].parse::<u8>().ok())
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(v: serde_json::Value) -> String {
        format!("{v}\n")
    }

    #[test]
    fn hop_counts_only_the_current_turn_and_line_start_headers() {
        let prev = line(serde_json::json!({"type":"user","message":{"content":"[context: x]\n\n[agent-msg from:task:ab at:t hop:1]\nold"}}));
        let end = line(serde_json::json!({"type":"system","subtype":"turn_duration"}));
        let op = line(serde_json::json!({"type":"user","message":{"content":"[WhatsApp — chat c — ref:wa-00000000]\n\nquote: [agent-msg from:x at:y hop:1] inline"}}));
        assert_eq!(current_turn_hop_of(&format!("{prev}{end}{op}")), 0);
        let absorbed = line(serde_json::json!({"type":"attachment","attachment":{"type":"queued_command","prompt":"[context: x]\n\n[agent-msg from:main at:t hop:1]\nbody"}}));
        assert_eq!(current_turn_hop_of(&format!("{prev}{end}{op}{absorbed}")), 1);
        // A tool result that merely contains a header is not an input.
        let tool = line(serde_json::json!({"type":"user","message":{"content":[{"type":"tool_result","content":"[agent-msg from:a at:b hop:1]"}]}}));
        assert_eq!(current_turn_hop_of(&format!("{end}{tool}")), 0);
        // No turn end yet: the whole transcript is the current turn.
        assert_eq!(current_turn_hop_of(&prev), 1);
    }

    #[test]
    fn scope_hash_is_hex_sha256() {
        assert_eq!(
            scope_token_hash("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[tokio::test]
    async fn roles_from_scope_token_and_session_ids() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        // whatsapp.db as the bot creates it (only the tables read here).
        let wa = crate::db::open(&dir.path().join(WHATSAPP_DB_PATH)).await.unwrap();
        sqlx::query("CREATE TABLE chat_sessions (chat_id TEXT PRIMARY KEY, session_id TEXT NOT NULL)")
            .execute(&wa)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE task_scopes (token_sha256 TEXT PRIMARY KEY, chat_id TEXT NOT NULL, created_at TEXT NOT NULL)")
            .execute(&wa)
            .await
            .unwrap();
        sqlx::query("INSERT INTO chat_sessions VALUES ('5511999999999@s.whatsapp.net', 'sess-chat')")
            .execute(&wa)
            .await
            .unwrap();
        sqlx::query("INSERT INTO task_scopes VALUES (?1, '5511999999999@s.whatsapp.net', 'x')")
            .bind(scope_token_hash("tok"))
            .execute(&wa)
            .await
            .unwrap();
        let tasks = crate::tasks::open(dir.path()).await.unwrap();
        sqlx::query(
            "INSERT INTO tasks (id, kind, title, brief, origin, requested_by, status, created_at, session_id)
             VALUES ('t1', 'general', 't', 'b', 'cli', 'cli', 'running', 'x', 'sess-worker')",
        )
        .execute(&tasks)
        .await
        .unwrap();

        use crate::proc_tree::NucleusSession;
        let op = Origin::Terminal;
        let nucleus = |kind: &str, scope: Option<&str>, worker: Option<&str>| {
            Origin::Nucleus(NucleusSession {
                pid: 10,
                kind: kind.into(),
                agent: Some("whatsapp".into()),
                scope: scope.map(str::to_string),
                worker: worker.map(str::to_string),
                session_id: None,
            })
        };
        let ws = dir.path();
        // The operator, unless the environment's session id says otherwise
        // (restrictive only).
        assert_eq!(role_for(ws, &op, Some("sess-other")).await, Role::Operator);
        assert_eq!(role_for(ws, &op, Some("sess-chat")).await, Role::UnscopedChat);
        assert_eq!(role_for(ws, &op, Some("sess-worker")).await, Role::Worker { task_id: None });
        // Process-tree origins: the scope comes from the claude start
        // environment and must be recorded.
        assert_eq!(
            role_for(ws, &nucleus("chat", Some("tok"), None), None).await,
            Role::Chat { origin: "whatsapp-dm".into(), chat: "5511999999999@s.whatsapp.net".into() }
        );
        assert_eq!(role_for(ws, &nucleus("chat", Some("forged"), None), None).await, Role::UnscopedChat);
        assert_eq!(role_for(ws, &nucleus("chat", None, None), None).await, Role::UnscopedChat);
        assert_eq!(
            role_for(ws, &nucleus("worker", None, Some("ab12")), None).await,
            Role::Worker { task_id: Some("ab12".into()) }
        );
        // A Nucleus session never falls back to the operator.
        assert_eq!(role_for(ws, &nucleus("agent", Some("tok"), None), None).await, Role::Session { kind: "agent".into() });
        assert_eq!(role_for(ws, &Origin::Detached, None).await, Role::Detached);
        assert!(matches!(role_for(ws, &Origin::Unknown("x".into()), None).await, Role::Unknown(_)));
        assert_eq!(
            scope_chat(dir.path(), "tok").await.as_deref(),
            Some("5511999999999@s.whatsapp.net")
        );
        assert_eq!(scope_chat(dir.path(), "forged").await, None);
    }
}
