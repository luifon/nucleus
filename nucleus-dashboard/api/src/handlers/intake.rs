//! Intake surface (ADR-036): the issue pipeline's items.
//!
//!   GET  /intake/api/list?all=          — items, newest first (open only unless all)
//!   GET  /intake/api/detail?id=         — one item: event, eval, thread, tasks, stage log
//!   POST /intake/api/reply {id,text}    — operator message in the item's thread
//!   POST /intake/api/approve-plan {id,version}
//!   POST /intake/api/approve-comment {id,text?}
//!   POST /intake/api/skip-comment {id}
//!   POST /intake/api/cancel {id}
//!   POST /intake/api/retry {id}
//!
//! Reads open intake.db and tasks.db read-only per request and treat a
//! missing DB as empty. Writes go through `nucleus_core::intake::pipeline`
//! (the module that owns intake.db); after a write the handler starts
//! `nucleus intake tick` detached so the pipeline acts at once instead of
//! at the next minute.
//!
//! Threat model: as for the Tasks page (ADR-033 §7). The dashboard is on the
//! tailnet only and acts with the operator's scope. Every write accepts a
//! JSON body only and refuses a request a browser marks as cross-site. A
//! plan approval names the version the operator saw; the pipeline refuses
//! it when a newer plan exists or the agent is still answering.

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use nucleus_core::intake::pipeline::{self, Ctx, Refusal};
use nucleus_core::intake::stage::EvalResult;
use nucleus_core::intake::{store, Event, Item, ItemMessage, ItemTransition};
use nucleus_core::tasks::{self, Task};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

pub struct IntakeState {
    pub workspace_root: PathBuf,
    pub intake: nucleus_core::config::IntakeConfig,
    pub tasks: nucleus_core::config::TasksConfig,
}

pub fn router(state: Arc<IntakeState>) -> Router {
    Router::new()
        .route("/list", get(list))
        .route("/detail", get(detail))
        .route("/reply", post(reply))
        .route("/approve-plan", post(approve_plan))
        .route("/approve-comment", post(approve_comment))
        .route("/skip-comment", post(skip_comment))
        .route("/cancel", post(cancel))
        .route("/retry", post(retry))
        .with_state(state)
}

// ─── DTOs ──────────────────────────────────────────────────────────────────

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct IntakeDetail {
    item: Item,
    event: Event,
    eval: Option<EvalResult>,
    messages: Vec<ItemMessage>,
    /// Every stage task of the item, oldest first (from the task ledger).
    tasks: Vec<Task>,
    transitions: Vec<ItemTransition>,
}

#[derive(Deserialize)]
struct ListQ {
    all: Option<bool>,
}

#[derive(Deserialize)]
struct DetailQ {
    id: i64,
}

#[derive(Deserialize, ts_rs::TS)]
#[ts(export)]
struct IntakeReplyReq {
    #[ts(type = "number")]
    id: i64,
    text: String,
}

#[derive(Deserialize, ts_rs::TS)]
#[ts(export)]
struct IntakeApprovePlanReq {
    #[ts(type = "number")]
    id: i64,
    /// The plan version the operator read.
    version: u32,
}

#[derive(Deserialize, ts_rs::TS)]
#[ts(export)]
struct IntakeApproveCommentReq {
    #[ts(type = "number")]
    id: i64,
    /// Replacement text; the proposed text when absent.
    #[ts(optional)]
    text: Option<String>,
}

#[derive(Deserialize, ts_rs::TS)]
#[ts(export)]
struct IntakeItemReq {
    #[ts(type = "number")]
    id: i64,
}

// ─── handlers ──────────────────────────────────────────────────────────────

async fn read_pool(s: &IntakeState) -> Option<sqlx::SqlitePool> {
    // Intake off, or never run: nothing to show (and nothing is created).
    if !s.intake.enabled || !s.workspace_root.join(nucleus_core::intake::INTAKE_DB_PATH).exists() {
        return None;
    }
    store::open_read_only(&s.workspace_root).await.ok()
}

async fn list(State(s): State<Arc<IntakeState>>, Query(q): Query<ListQ>) -> Result<Json<Vec<Item>>, IntakeError> {
    let Some(pool) = read_pool(&s).await else { return Ok(Json(vec![])) };
    let rows = store::list_items(&pool, !q.all.unwrap_or(true), 300).await.map_err(IntakeError::other)?;
    Ok(Json(rows))
}

async fn detail(State(s): State<Arc<IntakeState>>, Query(q): Query<DetailQ>) -> Result<Json<IntakeDetail>, IntakeError> {
    let pool = read_pool(&s).await.ok_or(IntakeError::NotFound(q.id))?;
    let item = store::item(&pool, q.id).await.map_err(|_| IntakeError::NotFound(q.id))?;
    let event = store::event(&pool, item.event_id).await.map_err(IntakeError::other)?;
    let eval = item.eval_json.as_deref().and_then(|j| serde_json::from_str(j).ok());
    let messages = store::messages(&pool, item.id).await.map_err(IntakeError::other)?;
    let transitions = store::transitions(&pool, item.id).await.map_err(IntakeError::other)?;
    let mut task_rows = Vec::new();
    if s.workspace_root.join(tasks::TASKS_DB_PATH).exists() {
        if let Ok(tp) = tasks::open_read_only(&s.workspace_root).await {
            for t in store::item_tasks(&pool, item.id).await.map_err(IntakeError::other)? {
                if let Ok(task) = tasks::get(&tp, &t.task_id, &tasks::Scope::Operator).await {
                    task_rows.push(task);
                }
            }
        }
    }
    Ok(Json(IntakeDetail { item, event, eval, messages, tasks: task_rows, transitions }))
}

fn same_origin(headers: &HeaderMap) -> Result<(), IntakeError> {
    match super::tasks::cross_site_reason(headers) {
        Some(why) => Err(IntakeError::Forbidden(why)),
        None => Ok(()),
    }
}

async fn ctx(s: &IntakeState) -> Result<Ctx, IntakeError> {
    if !s.intake.enabled {
        return Err(IntakeError::Conflict("intake is disabled ([intake] enabled = false)".into()));
    }
    let ws = &s.workspace_root;
    let need_gh = !s.intake.repos.is_empty();
    let tools = nucleus_core::intake::tools::ToolPins::pin(&s.intake.github.gh_bin, need_gh).map_err(IntakeError::other)?;
    let gh: Arc<dyn nucleus_core::intake::github::GhRunner> = match &tools.gh {
        Some(pin) => Arc::new(nucleus_core::intake::github::GhCli { pin: pin.clone() }),
        None => Arc::new(nucleus_core::intake::github::NoGh),
    };
    Ok(Ctx {
        ws: ws.clone(),
        cfg: s.intake.clone(),
        tasks_cfg: s.tasks.clone(),
        db: store::open(ws).await.map_err(IntakeError::other)?,
        tasks_db: tasks::open(ws).await.map_err(IntakeError::other)?,
        wa: nucleus_core::whatsapp_queue::open(ws).await.map_err(IntakeError::other)?,
        gh,
        launcher: Arc::new(pipeline::WorkerLauncher),
        guard: Arc::new(nucleus_core::intake::publish::ScriptGuard { workspace_root: ws.clone() }),
        tools: Arc::new(tools),
        viewer: tokio::sync::OnceCell::new(),
    })
}

/// Start `nucleus intake tick` detached, so a decision takes effect now.
fn kick_tick(ws: &std::path::Path) {
    let Ok(exe) = std::env::current_exe() else { return };
    let spawned = std::process::Command::new(exe)
        .args(["intake", "tick"])
        .current_dir(ws)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Err(e) = spawned {
        tracing::warn!("intake: starting a tick failed: {e}");
    }
}

fn outcome(r: anyhow::Result<Item>, ws: &std::path::Path) -> Result<Json<Item>, IntakeError> {
    match r {
        Ok(item) => {
            kick_tick(ws);
            Ok(Json(item))
        }
        Err(e) if e.downcast_ref::<Refusal>().is_some() => Err(IntakeError::Conflict(e.to_string())),
        Err(e) => Err(IntakeError::Conflict(format!("{e:#}"))),
    }
}

async fn reply(
    State(s): State<Arc<IntakeState>>,
    headers: HeaderMap,
    Json(req): Json<IntakeReplyReq>,
) -> Result<Json<Item>, IntakeError> {
    same_origin(&headers)?;
    let c = ctx(&s).await?;
    outcome(pipeline::reply(&c, req.id, &req.text, "dashboard").await, &s.workspace_root)
}

async fn approve_plan(
    State(s): State<Arc<IntakeState>>,
    headers: HeaderMap,
    Json(req): Json<IntakeApprovePlanReq>,
) -> Result<Json<Item>, IntakeError> {
    same_origin(&headers)?;
    let c = ctx(&s).await?;
    outcome(pipeline::approve_plan(&c, req.id, Some(req.version), "dashboard").await, &s.workspace_root)
}

async fn approve_comment(
    State(s): State<Arc<IntakeState>>,
    headers: HeaderMap,
    Json(req): Json<IntakeApproveCommentReq>,
) -> Result<Json<Item>, IntakeError> {
    same_origin(&headers)?;
    let c = ctx(&s).await?;
    outcome(pipeline::approve_comment(&c, req.id, req.text, "dashboard").await, &s.workspace_root)
}

async fn skip_comment(
    State(s): State<Arc<IntakeState>>,
    headers: HeaderMap,
    Json(req): Json<IntakeItemReq>,
) -> Result<Json<Item>, IntakeError> {
    same_origin(&headers)?;
    let c = ctx(&s).await?;
    outcome(pipeline::skip_comment(&c, req.id, "dashboard").await, &s.workspace_root)
}

async fn cancel(
    State(s): State<Arc<IntakeState>>,
    headers: HeaderMap,
    Json(req): Json<IntakeItemReq>,
) -> Result<Json<Item>, IntakeError> {
    same_origin(&headers)?;
    let c = ctx(&s).await?;
    outcome(pipeline::cancel(&c, req.id, "dashboard").await, &s.workspace_root)
}

async fn retry(
    State(s): State<Arc<IntakeState>>,
    headers: HeaderMap,
    Json(req): Json<IntakeItemReq>,
) -> Result<Json<Item>, IntakeError> {
    same_origin(&headers)?;
    let c = ctx(&s).await?;
    outcome(pipeline::retry(&c, req.id, "dashboard").await, &s.workspace_root)
}

// ─── errors ────────────────────────────────────────────────────────────────

#[derive(Debug)]
enum IntakeError {
    NotFound(i64),
    Conflict(String),
    Forbidden(String),
    Other(String),
}

impl IntakeError {
    fn other(e: impl std::fmt::Display) -> Self {
        Self::Other(e.to_string())
    }
}

impl IntoResponse for IntakeError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::NotFound(id) => (StatusCode::NOT_FOUND, format!("item #{id} not found")),
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
    use tower::ServiceExt;

    fn state(dir: &std::path::Path) -> Arc<IntakeState> {
        Arc::new(IntakeState {
            workspace_root: dir.to_path_buf(),
            intake: Default::default(),
            tasks: Default::default(),
        })
    }

    #[tokio::test]
    async fn a_missing_database_is_an_empty_list() {
        let dir = tempfile::tempdir().unwrap();
        let Json(l) = list(State(state(dir.path())), Query(ListQ { all: Some(true) })).await.unwrap();
        assert!(l.is_empty());
        assert!(detail(State(state(dir.path())), Query(DetailQ { id: 1 })).await.is_err());
    }

    #[tokio::test]
    async fn disabled_intake_shows_nothing_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        let app = router(state(dir.path()));
        let list_req = axum::http::Request::get("/list?all=true").body(axum::body::Body::empty()).unwrap();
        let res = app.clone().oneshot(list_req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
        assert_eq!(&body[..], b"[]");
        let cancel = axum::http::Request::post("/cancel")
            .header("content-type", "application/json")
            .header("sec-fetch-site", "same-origin")
            .body(axum::body::Body::from(r#"{"id":1}"#))
            .unwrap();
        assert_eq!(app.oneshot(cancel).await.unwrap().status(), StatusCode::CONFLICT);
        assert!(!dir.path().join(nucleus_core::intake::INTAKE_DB_PATH).exists(), "no intake.db is created");
    }

    fn enabled_state(dir: &std::path::Path) -> Arc<IntakeState> {
        let mut intake = nucleus_core::config::IntakeConfig { enabled: true, ..Default::default() };
        intake.github.gh_bin = "sh".into();
        Arc::new(IntakeState { workspace_root: dir.to_path_buf(), intake, tasks: Default::default() })
    }

    #[tokio::test]
    async fn writes_need_json_and_the_same_origin() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        let app = router(enabled_state(dir.path()));
        let form = axum::http::Request::post("/approve-plan")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(axum::body::Body::from("id=1&version=1"))
            .unwrap();
        assert_eq!(app.clone().oneshot(form).await.unwrap().status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        let cross = axum::http::Request::post("/cancel")
            .header("content-type", "application/json")
            .header("sec-fetch-site", "cross-site")
            .body(axum::body::Body::from(r#"{"id":1}"#))
            .unwrap();
        assert_eq!(app.clone().oneshot(cross).await.unwrap().status(), StatusCode::FORBIDDEN);
        // Same origin, but no such item: a conflict with the reason, not a crash.
        let ok = axum::http::Request::post("/cancel")
            .header("content-type", "application/json")
            .header("sec-fetch-site", "same-origin")
            .body(axum::body::Body::from(r#"{"id":1}"#))
            .unwrap();
        assert_eq!(app.oneshot(ok).await.unwrap().status(), StatusCode::CONFLICT);
    }
}
