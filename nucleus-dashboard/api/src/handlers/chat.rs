//! Chat surface — multi-chat against the Obsidian vault, lifted
//! from the standalone `chat/` crate (ADR-015 Phase 1). Each chat
//! is a Claude session resumed across messages. Messages are
//! double-stored (in our SQLite + Claude's session transcript) so
//! we can render history independently of Claude's internal session
//! files.
//!
//! Routes:
//!   - GET    /chat/api/chats        list all chats (most-recent first)
//!   - POST   /chat/api/chats        create a new empty chat
//!   - GET    /chat/api/chats/:id    chat metadata + full message history
//!   - DELETE /chat/api/chats/:id    remove chat + its messages
//!   - POST   /chat/api/chats/:id/messages   ask the session

use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use chrono::Utc;
use nucleus_core::{
    claude::PermissionMode,
    claude_session::{AskOptions, Session, SessionPool, SpawnOptions},
    diary,
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::Arc;

const AGENT_NAME: &str = "chat";

pub struct ChatState {
    pub pool: SqlitePool,
    pub vault_path: PathBuf,
    pub workspace_root: PathBuf,
    pub sessions: SessionPool,
}

pub fn router(state: Arc<ChatState>) -> Router {
    Router::new()
        .route("/chats", get(list_chats).post(create_chat))
        .route("/chats/{id}", get(get_chat).delete(delete_chat))
        .route("/chats/{id}/messages", post(send_message))
        .with_state(state)
}

pub async fn ensure_schema(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS obsidian_chats (
          id TEXT PRIMARY KEY,
          title TEXT,
          claude_session_id TEXT,
          created_at TEXT NOT NULL,
          last_active TEXT NOT NULL
        );
    "#,
    )
    .execute(pool)
    .await?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS obsidian_messages (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          chat_id TEXT NOT NULL REFERENCES obsidian_chats(id),
          role TEXT NOT NULL,
          content TEXT NOT NULL,
          ts TEXT NOT NULL
        );
    "#,
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_obs_msgs_chat ON obsidian_messages(chat_id)")
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Serialize, sqlx::FromRow)]
pub struct ChatRow {
    pub id: String,
    pub title: Option<String>,
    pub claude_session_id: Option<String>,
    pub created_at: String,
    pub last_active: String,
}

#[derive(Serialize, sqlx::FromRow)]
pub struct MessageRow {
    pub id: i64,
    pub chat_id: String,
    pub role: String,
    pub content: String,
    pub ts: String,
}

async fn list_chats(State(s): State<Arc<ChatState>>) -> Result<Json<Vec<ChatRow>>, ChatError> {
    let rows: Vec<ChatRow> = sqlx::query_as::<_, ChatRow>(
        "SELECT id, title, claude_session_id, created_at, last_active
           FROM obsidian_chats
          ORDER BY last_active DESC
          LIMIT 100",
    )
    .fetch_all(&s.pool)
    .await?;
    Ok(Json(rows))
}

#[derive(Serialize)]
struct CreatedChat {
    id: String,
    created_at: String,
}

async fn create_chat(State(s): State<Arc<ChatState>>) -> Result<Json<CreatedChat>, ChatError> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    sqlx::query("INSERT INTO obsidian_chats (id, created_at, last_active) VALUES (?1, ?2, ?2)")
        .bind(&id)
        .bind(&now)
        .execute(&s.pool)
        .await?;
    Ok(Json(CreatedChat { id, created_at: now }))
}

#[derive(Serialize)]
struct ChatDetail {
    chat: ChatRow,
    messages: Vec<MessageRow>,
}

async fn get_chat(
    State(s): State<Arc<ChatState>>,
    Path(id): Path<String>,
) -> Result<Json<ChatDetail>, ChatError> {
    let chat: Option<ChatRow> = sqlx::query_as::<_, ChatRow>(
        "SELECT id, title, claude_session_id, created_at, last_active
           FROM obsidian_chats WHERE id = ?1",
    )
    .bind(&id)
    .fetch_optional(&s.pool)
    .await?;
    let chat = chat.ok_or(ChatError::NotFound)?;
    let messages: Vec<MessageRow> = sqlx::query_as::<_, MessageRow>(
        "SELECT id, chat_id, role, content, ts
           FROM obsidian_messages
          WHERE chat_id = ?1
          ORDER BY id ASC",
    )
    .bind(&id)
    .fetch_all(&s.pool)
    .await?;
    Ok(Json(ChatDetail { chat, messages }))
}

async fn delete_chat(
    State(s): State<Arc<ChatState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ChatError> {
    sqlx::query("DELETE FROM obsidian_messages WHERE chat_id = ?1")
        .bind(&id)
        .execute(&s.pool)
        .await?;
    let res = sqlx::query("DELETE FROM obsidian_chats WHERE id = ?1")
        .bind(&id)
        .execute(&s.pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(ChatError::NotFound);
    }
    Ok(Json(serde_json::json!({ "ok": true, "deleted": id })))
}

#[derive(Deserialize)]
struct SendReq {
    message: String,
}

#[derive(Serialize)]
struct SendResp {
    user_message: MessageRow,
    assistant_message: MessageRow,
    chat_title: Option<String>,
    session_id: String,
}

async fn send_message(
    State(s): State<Arc<ChatState>>,
    Path(id): Path<String>,
    Json(req): Json<SendReq>,
) -> Result<Json<SendResp>, ChatError> {
    let chat: Option<ChatRow> = sqlx::query_as::<_, ChatRow>(
        "SELECT id, title, claude_session_id, created_at, last_active
           FROM obsidian_chats WHERE id = ?1",
    )
    .bind(&id)
    .fetch_optional(&s.pool)
    .await?;
    let chat = chat.ok_or(ChatError::NotFound)?;
    if req.message.trim().is_empty() {
        return Err(ChatError::BadRequest("message empty".into()));
    }

    // Capture the user-msg timestamp now (the moment the request arrived)
    // but defer the INSERT until after sessions.ask() succeeds. If ask()
    // errors (claude session crashed, transcript timeout, etc.), nothing
    // hits the DB — no orphan user row left over from a failed turn.
    let user_now = Utc::now().to_rfc3339();

    let prompt = format!(
        "You are answering a question against the Obsidian vault at {:?}. The vault \
         is mounted via --add-dir. Read files when relevant, cite paths. Lead with the \
         answer. Brief, no narration.\n\nQuestion: {}",
        s.vault_path, req.message
    );

    let ask_result = s
        .sessions
        .ask(&id, &prompt, chat.claude_session_id.clone(), AskOptions::default())
        .await
        .map_err(|e| ChatError::Internal(e.to_string()))?;

    let assistant_now = Utc::now().to_rfc3339();
    let mut tx = s.pool.begin().await?;

    sqlx::query("INSERT INTO obsidian_messages (chat_id, role, content, ts) VALUES (?1, 'user', ?2, ?3)")
        .bind(&id)
        .bind(&req.message)
        .bind(&user_now)
        .execute(&mut *tx)
        .await?;
    let (user_msg_id,): (i64,) = sqlx::query_as("SELECT last_insert_rowid()")
        .fetch_one(&mut *tx)
        .await?;

    sqlx::query("INSERT INTO obsidian_messages (chat_id, role, content, ts) VALUES (?1, 'assistant', ?2, ?3)")
        .bind(&id)
        .bind(&ask_result.reply)
        .bind(&assistant_now)
        .execute(&mut *tx)
        .await?;
    let (asst_msg_id,): (i64,) = sqlx::query_as("SELECT last_insert_rowid()")
        .fetch_one(&mut *tx)
        .await?;

    sqlx::query("UPDATE obsidian_chats SET claude_session_id = ?1, last_active = ?2 WHERE id = ?3")
        .bind(&ask_result.session_id)
        .bind(&assistant_now)
        .bind(&id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    // Auto-title after first round-trip via a separate one-shot session so
    // it doesn't pollute the main chat's context.
    let mut new_title = chat.title.clone();
    if chat.title.is_none() {
        if let Ok(title) = generate_title(&s.workspace_root, &req.message, &ask_result.reply).await {
            sqlx::query("UPDATE obsidian_chats SET title = ?1 WHERE id = ?2")
                .bind(&title)
                .bind(&id)
                .execute(&s.pool)
                .await?;
            new_title = Some(title);
        }
    }

    let _ = diary::record_observation(
        &s.workspace_root,
        AGENT_NAME,
        &format!("chat {}", &id[..8]),
        &format!(
            "user msg ({}c) → reply ({}c) in {:.1}s — session {}",
            req.message.len(),
            ask_result.reply.len(),
            ask_result.elapsed.as_secs_f64(),
            ask_result.session_id
        ),
        diary::Tag::Observation,
    );

    Ok(Json(SendResp {
        user_message: MessageRow {
            id: user_msg_id,
            chat_id: id.clone(),
            role: "user".into(),
            content: req.message,
            ts: user_now,
        },
        assistant_message: MessageRow {
            id: asst_msg_id,
            chat_id: id,
            role: "assistant".into(),
            content: ask_result.reply.clone(),
            ts: assistant_now,
        },
        chat_title: new_title,
        session_id: ask_result.session_id,
    }))
}

async fn generate_title(cwd: &PathBuf, user: &str, assistant: &str) -> Result<String> {
    // chars().take() — slicing by byte offset can panic on multi-byte UTF-8
    // boundaries (common in PT/ES/JP/CJK content).
    let user_clip: String = user.chars().take(400).collect();
    let asst_clip: String = assistant.chars().take(400).collect();
    let prompt = format!(
        "Generate a 3-6 word title for this chat. Output only the title, no quotes, no punctuation at end.\n\nUser: {}\n\nAssistant: {}",
        user_clip, asst_clip
    );
    let mut session = Session::spawn(SpawnOptions {
        workspace_root: cwd.clone(),
        permission_mode: Some(PermissionMode::Auto),
        tmux_session: "nucleus-chat-title".into(),
        window_name: Some("title".into()),
        ..SpawnOptions::default()
    })
    .await?;
    let raw = session.ask(&prompt, AskOptions::default()).await?;
    let _ = session.close().await;
    let title = raw.trim().trim_matches('"').trim_matches('\'').to_string();
    Ok(title.chars().take(80).collect())
}

#[derive(Debug)]
pub enum ChatError {
    Sqlx(sqlx::Error),
    NotFound,
    BadRequest(String),
    Internal(String),
}

impl From<sqlx::Error> for ChatError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sqlx(e)
    }
}

impl From<anyhow::Error> for ChatError {
    fn from(e: anyhow::Error) -> Self {
        Self::Internal(e.to_string())
    }
}

impl IntoResponse for ChatError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "not found".into()),
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            Self::Sqlx(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {}", e)),
            Self::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}
