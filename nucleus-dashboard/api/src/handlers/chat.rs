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
    claude_session::{AskOptions, AskResult, SessionPool},
    config::ClaudeConfig,
    diary,
    session_profile::{ProfileContext, SessionProfile},
};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

const AGENT_NAME: &str = "chat";

pub struct ChatState {
    pub pool: SqlitePool,
    pub vault_path: PathBuf,
    pub workspace_root: PathBuf,
    pub sessions: SessionPool,
    /// Profile-derived per-turn ask options (ADR-020).
    pub ask_options: AskOptions,
    /// Settings posture for the one-shot title generator.
    pub claude: ClaudeConfig,
    pub persona_display_name: String,
    /// Turns and titles running for a chat after their request returned.
    pub background: Background,
    /// Held for writing while a turn commits its messages and updates
    /// `background`, and for reading while `get_chat` reads both, so a read
    /// sees the chat either before the commit or after it.
    pub commit_gate: tokio::sync::RwLock<()>,
}

/// Work the server runs for a chat in tasks of its own: one turn and one
/// title at most per chat. In memory only; a dashboard restart ends the
/// tasks too. Cloning shares the state.
#[derive(Clone, Default)]
pub struct Background(Arc<Mutex<BackgroundState>>);

#[derive(Default)]
struct BackgroundState {
    /// The last turn of each chat that is not stored as messages: running,
    /// or failed (until the next message replaces it).
    turns: HashMap<String, PendingTurn>,
    /// Chats whose title is being generated.
    titling: HashSet<String>,
}

/// A turn that is not stored as messages.
#[derive(Clone, Debug, PartialEq, Serialize, ts_rs::TS)]
#[serde(tag = "status", rename_all = "snake_case")]
#[ts(export)]
pub enum PendingTurn {
    /// The session is answering `message`, sent at `ts`.
    Running { message: String, ts: String },
    /// The turn failed; nothing was stored and `message` was not answered.
    Failed { message: String, ts: String, error: String },
}

impl Background {
    fn lock(&self) -> MutexGuard<'_, BackgroundState> {
        // The state is plain maps, consistent after any panic.
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Record a running turn for `chat`. `None` while one already runs.
    fn start_turn(&self, chat: &str, message: &str, ts: &str) -> Option<TurnGuard> {
        let mut st = self.lock();
        if matches!(st.turns.get(chat), Some(PendingTurn::Running { .. })) {
            return None;
        }
        st.turns.insert(
            chat.to_string(),
            PendingTurn::Running { message: message.to_string(), ts: ts.to_string() },
        );
        Some(TurnGuard {
            bg: self.clone(),
            chat: chat.to_string(),
            message: message.to_string(),
            ts: ts.to_string(),
            done: false,
        })
    }

    /// Mark `chat` as being titled. `None` while a title already runs.
    fn start_titling(&self, chat: &str) -> Option<TitlingGuard> {
        self.lock()
            .titling
            .insert(chat.to_string())
            .then(|| TitlingGuard { bg: self.clone(), chat: chat.to_string() })
    }

    /// The chat's pending turn and whether a title is running.
    fn snapshot(&self, chat: &str) -> (Option<PendingTurn>, bool) {
        let st = self.lock();
        (st.turns.get(chat).cloned(), st.titling.contains(chat))
    }

    /// Forget a failed turn (its chat was deleted). A running turn removes
    /// its own entry when it ends.
    fn forget_failed(&self, chat: &str) {
        let mut st = self.lock();
        if matches!(st.turns.get(chat), Some(PendingTurn::Failed { .. })) {
            st.turns.remove(chat);
        }
    }
}

/// Sets the final state of a turn. Dropped without `finish` (the task
/// panicked), it marks the turn failed.
struct TurnGuard {
    bg: Background,
    chat: String,
    message: String,
    ts: String,
    done: bool,
}

impl TurnGuard {
    /// `None`: the turn is stored (or its chat is gone); `Some(error)`: it failed.
    fn finish(&mut self, error: Option<String>) {
        self.done = true;
        let mut st = self.bg.lock();
        match error {
            None => {
                st.turns.remove(&self.chat);
            }
            Some(error) => {
                st.turns.insert(
                    self.chat.clone(),
                    PendingTurn::Failed { message: self.message.clone(), ts: self.ts.clone(), error },
                );
            }
        }
    }
}

impl Drop for TurnGuard {
    fn drop(&mut self) {
        if !self.done {
            self.finish(Some("the reply task ended without a result".into()));
        }
    }
}

/// Removes the chat from `titling` when the title task ends, panics included.
struct TitlingGuard {
    bg: Background,
    chat: String,
}

impl Drop for TitlingGuard {
    fn drop(&mut self) {
        self.bg.lock().titling.remove(&self.chat);
    }
}

pub fn router(state: Arc<ChatState>) -> Router {
    Router::new()
        .route("/info", get(get_info))
        .route("/chats", get(list_chats).post(create_chat))
        .route("/chats/{id}", get(get_chat).delete(delete_chat))
        .route("/chats/{id}/messages", post(send_message))
        .with_state(state)
}

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct ChatInfo {
    persona_name: String,
}

async fn get_info(State(s): State<Arc<ChatState>>) -> Json<ChatInfo> {
    Json(ChatInfo {
        persona_name: s.persona_display_name.clone(),
    })
}

/// Versioned migrations (ADR-020).
const MIGRATIONS: &[nucleus_core::migrate::Migration] = &[nucleus_core::migrate::Migration {
    version: 1,
    name: "baseline-obsidian-chat",
    step: nucleus_core::migrate::Step::Sql(
        r#"
        CREATE TABLE IF NOT EXISTS obsidian_chats (
          id TEXT PRIMARY KEY,
          title TEXT,
          claude_session_id TEXT,
          created_at TEXT NOT NULL,
          last_active TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS obsidian_messages (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          chat_id TEXT NOT NULL REFERENCES obsidian_chats(id),
          role TEXT NOT NULL,
          content TEXT NOT NULL,
          ts TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_obs_msgs_chat ON obsidian_messages(chat_id)
        "#,
    ),
}];

pub async fn ensure_schema(pool: &SqlitePool) -> Result<()> {
    nucleus_core::migrate::migrate(pool, MIGRATIONS).await?;
    Ok(())
}

#[derive(Serialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export)]
pub struct ChatRow {
    pub id: String,
    pub title: Option<String>,
    pub claude_session_id: Option<String>,
    pub created_at: String,
    pub last_active: String,
}

#[derive(Serialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export)]
pub struct MessageRow {
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
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

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
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

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct ChatDetail {
    chat: ChatRow,
    messages: Vec<MessageRow>,
    /// The chat's turn that is not stored as messages, if any.
    turn: Option<PendingTurn>,
    /// A title is being generated; `chat.title` changes when it ends.
    titling: bool,
}

async fn get_chat(
    State(s): State<Arc<ChatState>>,
    Path(id): Path<String>,
) -> Result<Json<ChatDetail>, ChatError> {
    let _gate = s.commit_gate.read().await;
    let (turn, titling) = s.background.snapshot(&id);
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
    Ok(Json(ChatDetail { chat, messages, turn, titling }))
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
    s.background.forget_failed(&id);
    Ok(Json(serde_json::json!({ "ok": true, "deleted": id })))
}

#[derive(Deserialize, ts_rs::TS)]
#[ts(export)]
struct SendReq {
    message: String,
}

/// Accept a message and return at once. The turn runs in a task of its own
/// (`run_turn`), so a page reload or a dropped connection cannot cancel it,
/// and the page reads the result with `GET /chats/{id}`.
async fn send_message(
    State(s): State<Arc<ChatState>>,
    Path(id): Path<String>,
    Json(req): Json<SendReq>,
) -> Result<(StatusCode, Json<PendingTurn>), ChatError> {
    if req.message.trim().is_empty() {
        return Err(ChatError::BadRequest("message empty".into()));
    }
    let chat: Option<ChatRow> = sqlx::query_as::<_, ChatRow>(
        "SELECT id, title, claude_session_id, created_at, last_active
           FROM obsidian_chats WHERE id = ?1",
    )
    .bind(&id)
    .fetch_optional(&s.pool)
    .await?;
    let chat = chat.ok_or(ChatError::NotFound)?;

    let ts = Utc::now().to_rfc3339();
    let turn = PendingTurn::Running { message: req.message.clone(), ts: ts.clone() };
    let guard = s
        .background
        .start_turn(&id, &req.message, &ts)
        .ok_or_else(|| ChatError::Conflict("a reply is still running in this chat".into()))?;
    tokio::spawn(run_turn(s, chat, req.message, guard));
    Ok((StatusCode::ACCEPTED, Json(turn)))
}

/// One chat turn: ask, store both messages, then the follow-up work. The
/// guard turns any exit that did not set a final state (a panic) into a
/// failed turn, so the page never waits on a task that no longer runs.
async fn run_turn(s: Arc<ChatState>, chat: ChatRow, message: String, mut guard: TurnGuard) {
    let id = chat.id.clone();
    let StoredTurn { ask: ask_result, titling } = match complete_turn(&s, &chat, &message, &mut guard).await {
        Ok(Some(stored)) => stored,
        // The chat was deleted while the turn ran; nothing to show.
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(chat = %id, "chat turn failed: {e:#}");
            guard.finish(Some(format!("{e:#}")));
            return;
        }
    };

    // On-the-fly skill review (ADR-017) — detached, fire-and-forget.
    if ask_result.review_due {
        nucleus_core::skills::fire_skill_review(
            &s.workspace_root,
            "chat",
            &id,
            &ask_result.transcript_path,
        );
    }

    if let Some(titling) = titling {
        spawn_title(s.clone(), titling, message.clone(), ask_result.reply.clone());
    }

    let _ = diary::record_observation(
        &s.workspace_root,
        AGENT_NAME,
        &format!("chat {}", &id[..8]),
        &format!(
            "user msg ({}c) → reply ({}c) in {:.1}s — session {}",
            message.len(),
            ask_result.reply.len(),
            ask_result.elapsed.as_secs_f64(),
            ask_result.session_id
        ),
        diary::Tag::Observation,
    );
}

/// A stored turn, and the title task's guard when the chat has no title.
struct StoredTurn {
    ask: AskResult,
    titling: Option<TitlingGuard>,
}

/// Ask the chat's session and store the user message and the reply in one
/// transaction. Both rows are written only after the ask returns, so a
/// failed ask leaves no orphan user row. Under the commit gate, the commit,
/// the start of the title and the removal of the running turn are one step
/// for `get_chat`: it never sees the reply without `titling`, or a running
/// turn beside its stored messages. `None` when the chat no longer exists at
/// commit time.
async fn complete_turn(
    s: &ChatState,
    chat: &ChatRow,
    message: &str,
    guard: &mut TurnGuard,
) -> Result<Option<StoredTurn>> {
    let prompt = format!(
        "You are answering a question against the Obsidian vault at {:?}. The vault \
         is mounted via --add-dir. Read files when relevant, cite paths. Lead with the \
         answer. Brief, no narration.\n\nQuestion: {}",
        s.vault_path, message
    );

    let ask_result = s
        .sessions
        .ask(&chat.id, &prompt, chat.claude_session_id.clone(), s.ask_options.clone())
        .await?;

    let assistant_now = Utc::now().to_rfc3339();
    let _gate = s.commit_gate.write().await;
    let mut tx = s.pool.begin().await?;

    let updated = sqlx::query(
        "UPDATE obsidian_chats SET claude_session_id = ?1, last_active = ?2 WHERE id = ?3",
    )
    .bind(&ask_result.session_id)
    .bind(&assistant_now)
    .bind(&chat.id)
    .execute(&mut *tx)
    .await?;
    if updated.rows_affected() == 0 {
        tx.rollback().await?;
        guard.finish(None);
        return Ok(None);
    }

    sqlx::query("INSERT INTO obsidian_messages (chat_id, role, content, ts) VALUES (?1, 'user', ?2, ?3)")
        .bind(&chat.id)
        .bind(message)
        .bind(&guard.ts)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO obsidian_messages (chat_id, role, content, ts) VALUES (?1, 'assistant', ?2, ?3)")
        .bind(&chat.id)
        .bind(&ask_result.reply)
        .bind(&assistant_now)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    let titling = if chat.title.is_none() { s.background.start_titling(&chat.id) } else { None };
    guard.finish(None);
    Ok(Some(StoredTurn { ask: ask_result, titling }))
}

/// Title the chat in a separate one-shot session, in a task of its own: it
/// keeps the title out of the chat's context, and the reply is already
/// readable while it runs. `titling` tells the page to keep polling.
fn spawn_title(s: Arc<ChatState>, guard: TitlingGuard, user: String, reply: String) {
    let id = guard.chat.clone();
    tokio::spawn(async move {
        let _guard = guard;
        match generate_title(&s, &user, &reply).await {
            Ok(title) if !title.is_empty() => {
                if let Err(e) = sqlx::query(
                    "UPDATE obsidian_chats SET title = ?1 WHERE id = ?2 AND title IS NULL",
                )
                .bind(&title)
                .bind(&id)
                .execute(&s.pool)
                .await
                {
                    tracing::warn!(chat = %id, "chat title: storing failed: {e:#}");
                }
            }
            Ok(_) => tracing::warn!(chat = %id, "chat title: the session returned an empty title"),
            Err(e) => tracing::warn!(chat = %id, "chat title failed: {e:#}"),
        }
    });
}

async fn generate_title(s: &ChatState, user: &str, assistant: &str) -> Result<String> {
    // Slice by chars, not bytes — byte slicing panics on multi-byte
    // UTF-8 boundaries (PT/ES/JP/CJK).
    let user_clip: String = user.chars().take(400).collect();
    let asst_clip: String = assistant.chars().take(400).collect();
    let prompt = format!(
        "Generate a 3-6 word title for this chat. Output only the title, no quotes, no punctuation at end.\n\nUser: {}\n\nAssistant: {}",
        user_clip, asst_clip
    );
    let outcome = SessionProfile::one_shot_utility(&ProfileContext {
        workspace_root: &s.workspace_root,
        claude: &s.claude,
        tmux_session: "nucleus-chat-title",
        agent_label: "chat",
    })
    .window_name("title")
    .run_one_shot(&prompt)
    .await?;
    let title = outcome.reply.trim().trim_matches('"').trim_matches('\'').to_string();
    Ok(title.chars().take(80).collect())
}

#[derive(Debug)]
pub enum ChatError {
    Sqlx(sqlx::Error),
    NotFound,
    BadRequest(String),
    Conflict(String),
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
            Self::Conflict(m) => (StatusCode::CONFLICT, m),
            Self::Sqlx(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {}", e)),
            Self::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_running_turn_per_chat() {
        let bg = Background::default();
        let mut first = bg.start_turn("c", "hi", "t1").expect("first turn starts");
        assert!(bg.start_turn("c", "again", "t2").is_none());
        assert!(bg.start_turn("other", "hi", "t3").is_some_and(|mut g| {
            g.finish(None);
            true
        }));
        first.finish(None);
        assert_eq!(bg.snapshot("c"), (None, false));
        assert!(bg.start_turn("c", "next", "t4").is_some_and(|mut g| {
            g.finish(None);
            true
        }));
    }

    #[test]
    fn a_failed_turn_keeps_its_message_and_allows_a_new_one() {
        let bg = Background::default();
        let mut g = bg.start_turn("c", "hi", "t1").unwrap();
        g.finish(Some("session wedged".into()));
        assert_eq!(
            bg.snapshot("c").0,
            Some(PendingTurn::Failed { message: "hi".into(), ts: "t1".into(), error: "session wedged".into() })
        );
        let mut retry = bg.start_turn("c", "hi", "t2").expect("a failed turn does not block");
        assert!(matches!(bg.snapshot("c").0, Some(PendingTurn::Running { .. })));
        retry.finish(None);
    }

    #[test]
    fn a_turn_dropped_without_finish_is_failed() {
        let bg = Background::default();
        drop(bg.start_turn("c", "hi", "t1").unwrap());
        assert!(matches!(bg.snapshot("c").0, Some(PendingTurn::Failed { .. })));
    }

    #[test]
    fn a_panicking_turn_task_is_failed() {
        let bg = Background::default();
        let g = bg.start_turn("c", "hi", "t1").unwrap();
        let joined = std::thread::spawn(move || {
            let _g = g;
            panic!("turn task panicked");
        })
        .join();
        assert!(joined.is_err());
        assert!(matches!(bg.snapshot("c").0, Some(PendingTurn::Failed { .. })));
    }

    #[test]
    fn titling_ends_with_its_guard() {
        let bg = Background::default();
        let g = bg.start_titling("c").expect("first title starts");
        assert!(bg.start_titling("c").is_none());
        assert!(bg.snapshot("c").1);
        drop(g);
        assert!(!bg.snapshot("c").1);
    }

    #[test]
    fn forget_failed_leaves_a_running_turn() {
        let bg = Background::default();
        let mut g = bg.start_turn("c", "hi", "t1").unwrap();
        bg.forget_failed("c");
        assert!(matches!(bg.snapshot("c").0, Some(PendingTurn::Running { .. })));
        g.finish(Some("e".into()));
        bg.forget_failed("c");
        assert_eq!(bg.snapshot("c").0, None);
    }
}
