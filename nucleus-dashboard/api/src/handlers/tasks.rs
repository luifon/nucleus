//! Tasks surface (ADR-033): background tasks and conversational turns.
//!
//!   GET  /tasks/api/list?all=     — tasks, newest first (active only unless all)
//!   GET  /tasks/api/detail?id=    — one task + its progress log + links
//!   POST /tasks/api/cancel {id}   — stop a task
//!   GET  /tasks/api/turns         — recent WhatsApp conversational turns
//!
//! Reads open tasks.db and whatsapp.db read-only per request
//! (`db::open_read_only`): both may not exist yet on a fresh machine, and a
//! missing DB is an empty list, not an error. The one write — cancel — goes
//! through `nucleus_core::tasks`, the module that owns every tasks.db write
//! (ADR-020 ownership as recorded in ADR-033). whatsapp.db belongs to the
//! TypeScript bot; this surface never writes it.
//!
//! Threat model (ADR-033 §7): the dashboard is reachable only on the tailnet
//! (ADR-011) and has no login; every device on the tailnet is the operator's.
//! The dashboard acts with the operator's scope. The cancel route accepts
//! only a JSON body (a cross-site form or `no-cors` request cannot produce
//! one) and refuses a request whose `Sec-Fetch-Site` or `Origin` shows it
//! came from another site, so a page on another origin that the operator
//! visits cannot cancel tasks.

use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use nucleus_core::tasks::{self, Task, TaskEvent, TaskLink};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

pub struct TasksState {
    pub workspace_root: PathBuf,
    pub tasks: nucleus_core::config::TasksConfig,
}

pub fn router(state: Arc<TasksState>) -> Router {
    Router::new()
        .route("/list", get(list))
        .route("/detail", get(detail))
        .route("/cancel", post(cancel))
        .route("/turns", get(turns))
        .with_state(state)
}

// ─── DTOs ──────────────────────────────────────────────────────────────────

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct TaskDetail {
    task: Task,
    events: Vec<TaskEvent>,
    links: Vec<TaskLink>,
}

#[derive(Deserialize)]
struct ListQ {
    all: Option<bool>,
}

#[derive(Deserialize)]
struct DetailQ {
    id: String,
}

#[derive(Deserialize, ts_rs::TS)]
#[ts(export)]
struct CancelTaskReq {
    id: String,
}

/// One conversational turn of the WhatsApp turn engine.
#[derive(Serialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export)]
struct TurnRow {
    id: String,
    /// "dm" | "group".
    pool: String,
    /// operator | autonomous | context | foreign
    kind: String,
    /// running | done | silent | failed | interrupted
    status: String,
    started_at: String,
    ended_at: Option<String>,
    #[ts(type = "number")]
    ack_sent: i64,
    #[ts(type = "number")]
    progress_count: i64,
    #[ts(type = "number | null")]
    reply_chars: Option<i64>,
    error: Option<String>,
    /// Operator messages the turn read.
    #[ts(type = "number")]
    inbound_count: i64,
    /// First operator message of the turn, cut to 120 characters.
    first_text: Option<String>,
}

// ─── handlers ──────────────────────────────────────────────────────────────

async fn read_pool(s: &TasksState) -> Option<sqlx::SqlitePool> {
    let path = s.workspace_root.join(tasks::TASKS_DB_PATH);
    if !path.exists() {
        return None;
    }
    tasks::open_read_only(&s.workspace_root).await.ok()
}

async fn list(
    State(s): State<Arc<TasksState>>,
    Query(q): Query<ListQ>,
) -> Result<Json<Vec<Task>>, TasksError> {
    let Some(pool) = read_pool(&s).await else { return Ok(Json(vec![])) };
    let rows = tasks::list(&pool, !q.all.unwrap_or(true), 200, &tasks::Scope::Operator)
        .await
        .map_err(TasksError::other)?;
    Ok(Json(rows))
}

async fn detail(
    State(s): State<Arc<TasksState>>,
    Query(q): Query<DetailQ>,
) -> Result<Json<TaskDetail>, TasksError> {
    let pool = read_pool(&s).await.ok_or_else(|| TasksError::NotFound(q.id.clone()))?;
    let task = tasks::get(&pool, &q.id, &tasks::Scope::Operator)
        .await
        .map_err(|_| TasksError::NotFound(q.id.clone()))?;
    let events = tasks::events(&pool, &task.id).await.map_err(TasksError::other)?;
    let links = tasks::links(&pool, &task.id).await.map_err(TasksError::other)?;
    Ok(Json(TaskDetail { task, events, links }))
}

/// Refuse a request a browser marks as cross-site. `Sec-Fetch-Site` is sent
/// by every current browser; `Origin` covers the rest. A request with
/// neither (curl on the tailnet) is not a browser request and carries no
/// ambient credentials to abuse.
fn require_same_origin(headers: &HeaderMap) -> Result<(), TasksError> {
    match cross_site_reason(headers) {
        Some(why) => Err(TasksError::Forbidden(why)),
        None => Ok(()),
    }
}

/// Why a request is cross-site, or `None` when it is not. Shared by every
/// mutating dashboard route that follows the ADR-033 §7 threat model (the
/// Tasks cancel, the Intake actions of ADR-036).
pub(crate) fn cross_site_reason(headers: &HeaderMap) -> Option<String> {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        if !matches!(site, "same-origin" | "none") {
            return Some(format!("cross-site request ({site})"));
        }
    }
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        let origin_host = origin.split("://").nth(1).unwrap_or("");
        let host = headers.get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
        if origin_host.is_empty() || origin_host != host {
            return Some(format!("origin {origin} is not this dashboard"));
        }
    }
    None
}

async fn cancel(
    State(s): State<Arc<TasksState>>,
    headers: HeaderMap,
    // `Json` rejects any request whose Content-Type is not application/json.
    Json(req): Json<CancelTaskReq>,
) -> Result<Json<Task>, TasksError> {
    require_same_origin(&headers)?;
    let pool = tasks::open(&s.workspace_root).await.map_err(TasksError::other)?;
    let t = tasks::request_cancel(&s.workspace_root, &pool, &s.tasks, &req.id, &tasks::Scope::Operator)
        .await
        .map_err(|e| TasksError::Conflict(format!("{e:#}")))?;
    Ok(Json(t))
}

async fn turns(State(s): State<Arc<TasksState>>) -> Result<Json<Vec<TurnRow>>, TasksError> {
    let path = s.workspace_root.join(nucleus_core::whatsapp_queue::WHATSAPP_DB_PATH);
    if !path.exists() {
        return Ok(Json(vec![]));
    }
    let pool = nucleus_core::db::open_read_only(&path).await.map_err(TasksError::other)?;
    // chat_turns appears once the ADR-033 bot has booted on this machine.
    let rows = sqlx::query_as::<_, TurnRow>(
        r#"
        SELECT t.id, t.pool, t.kind, t.status, t.started_at, t.ended_at, t.ack_sent,
               t.progress_count, t.reply_chars, t.error,
               (SELECT COUNT(*) FROM chat_inbound i WHERE i.turn_id = t.id) AS inbound_count,
               (SELECT substr(i.text_preview, 1, 120) FROM chat_inbound i
                 WHERE i.turn_id = t.id ORDER BY i.received_at LIMIT 1) AS first_text
          FROM chat_turns t
         ORDER BY t.started_at DESC
         LIMIT 100
        "#,
    )
    .fetch_all(&pool)
    .await
    .unwrap_or_default();
    Ok(Json(rows))
}

// ─── errors ────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum TasksError {
    NotFound(String),
    Conflict(String),
    Forbidden(String),
    Other(String),
}

impl TasksError {
    fn other(e: impl std::fmt::Display) -> Self {
        Self::Other(e.to_string())
    }
}

impl IntoResponse for TasksError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::NotFound(id) => (StatusCode::NOT_FOUND, format!("task {id:?} not found")),
            Self::Conflict(m) => (StatusCode::CONFLICT, m),
            Self::Forbidden(m) => (StatusCode::FORBIDDEN, m),
            Self::Other(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_databases_are_empty_lists() {
        let dir = tempfile::tempdir().unwrap();
        let s = Arc::new(TasksState {
            workspace_root: dir.path().to_path_buf(),
            tasks: Default::default(),
        });
        let Json(l) = list(State(s.clone()), Query(ListQ { all: Some(true) })).await.unwrap();
        assert!(l.is_empty());
        let Json(t) = turns(State(s)).await.unwrap();
        assert!(t.is_empty());
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        h
    }

    #[test]
    fn cancel_refuses_cross_site_requests() {
        assert!(require_same_origin(&headers(&[])).is_ok(), "curl on the tailnet");
        assert!(require_same_origin(&headers(&[("sec-fetch-site", "same-origin")])).is_ok());
        assert!(require_same_origin(&headers(&[("sec-fetch-site", "cross-site")])).is_err());
        assert!(require_same_origin(&headers(&[("sec-fetch-site", "same-site")])).is_err());
        assert!(require_same_origin(&headers(&[
            ("origin", "http://127.0.0.1:3000"),
            ("host", "127.0.0.1:3000")
        ]))
        .is_ok());
        assert!(require_same_origin(&headers(&[
            ("origin", "https://attacker.example"),
            ("host", "127.0.0.1:3000")
        ]))
        .is_err());
    }

    #[tokio::test]
    async fn cancel_route_requires_a_json_body() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        let s = Arc::new(TasksState { workspace_root: dir.path().to_path_buf(), tasks: Default::default() });
        let app = router(s);
        let req = axum::http::Request::post("/cancel")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(axum::body::Body::from("id=abcd"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
}
