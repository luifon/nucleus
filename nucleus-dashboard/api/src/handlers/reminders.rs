//! Reminders admin surface — full lifecycle management on top of
//! `reminders.db`, wrapping the same `reminders::store` helpers the
//! CLI uses (no SQL-shaped drift between the two surfaces).
//!
//! Read:
//!   - `GET  /reminders/api/list?include_fired=&include_cancelled=`
//!
//! Write (all narrow exceptions per ADR-015 §"Configuration discipline" —
//! these are CLI-driven operations the dashboard wraps; not config
//! editing):
//!   - `POST /reminders/api/pause    { id, until? }`
//!   - `POST /reminders/api/resume   { id }`
//!   - `POST /reminders/api/cancel   { id }`
//!   - `POST /reminders/api/set-title { id, title }`
//!
//! Each write returns the updated reminder so the frontend can
//! re-render in place without a second list fetch.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use chrono::{DateTime, Utc};
use reminders::store;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::sync::Arc;

#[derive(Clone)]
pub struct RemindersState {
    pub pool: SqlitePool,
}

pub fn router(state: Arc<RemindersState>) -> Router {
    Router::new()
        .route("/list", get(list))
        .route("/history", get(list_history))
        .route("/pause", post(pause))
        .route("/resume", post(resume))
        .route("/cancel", post(cancel))
        .route("/set-title", post(set_title))
        .with_state(state)
}

// ─── DTOs ──────────────────────────────────────────────────────────────────

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct ReminderView {
    #[serde(flatten)]
    inner: store::Reminder,
    channels: Vec<store::ChannelRow>,
}

#[derive(Deserialize, Default)]
struct ListQ {
    include_fired: Option<bool>,
    include_cancelled: Option<bool>,
}

async fn list(
    State(s): State<Arc<RemindersState>>,
    Query(q): Query<ListQ>,
) -> Result<Json<Vec<ReminderView>>, RemindersError> {
    let reminders = store::list_all(
        &s.pool,
        q.include_fired.unwrap_or(false),
        q.include_cancelled.unwrap_or(false),
    )
    .await
    .map_err(|e| RemindersError::Other(e.to_string()))?;
    let mut out = Vec::with_capacity(reminders.len());
    for r in reminders {
        let channels = store::channels_for(&s.pool, r.id)
            .await
            .map_err(|e| RemindersError::Other(e.to_string()))?;
        out.push(ReminderView { inner: r, channels });
    }
    Ok(Json(out))
}

// ─── fire history (folded in from the retired /cron surface) ─────────────────
//
// "Upcoming" needs no endpoint of its own — it's the active/pending rows of
// `/list`, which the frontend sorts by next_fire. Only the fire-attempt audit
// log was unique to /cron, so that's all that moves here.

#[derive(Serialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export)]
struct FireRow {
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    id: i64,
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    reminder_id: i64,
    fired_at: String,
    channel: String,
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    success: i64,
    msg_id: Option<String>,
    error: Option<String>,
    reminder_title: Option<String>,
    reminder_body: Option<String>,
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    is_skill_fire: i64,
}

/// Recent fire-attempt audit log, newest first (was `/cron/api/recent`) — the
/// one view `/reminders` lacked. Per-channel success/error.
async fn list_history(
    State(s): State<Arc<RemindersState>>,
) -> Result<Json<Vec<FireRow>>, RemindersError> {
    let rows = sqlx::query_as::<_, FireRow>(
        r#"
        SELECT f.id, f.reminder_id, f.fired_at, f.channel, f.success,
               f.msg_id, f.error,
               r.title AS reminder_title, r.body AS reminder_body,
               CASE WHEN r.system_prompt IS NOT NULL AND r.system_prompt != ''
                    THEN 1 ELSE 0 END AS is_skill_fire
        FROM reminder_fires f
        LEFT JOIN reminders r ON r.id = f.reminder_id
        ORDER BY f.fired_at DESC
        LIMIT 60
        "#,
    )
    .fetch_all(&s.pool)
    .await
    .map_err(|e| RemindersError::Other(e.to_string()))?;
    Ok(Json(rows))
}

// ─── write actions ─────────────────────────────────────────────────────────

#[derive(Deserialize, ts_rs::TS)]
#[ts(export)]
struct IdReq {
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    id: i64,
}

#[derive(Deserialize, ts_rs::TS)]
#[ts(export)]
struct PauseReq {
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    id: i64,
    /// ISO-8601 (any reasonable offset; UTC if no offset). When set
    /// the ticker auto-resumes at that time per ADR-006.
    until: Option<String>,
}

#[derive(Deserialize, ts_rs::TS)]
#[ts(export)]
struct SetTitleReq {
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    id: i64,
    /// Empty / null clears the title.
    title: Option<String>,
}

async fn pause(
    State(s): State<Arc<RemindersState>>,
    Json(req): Json<PauseReq>,
) -> Result<Json<ReminderView>, RemindersError> {
    let until_utc = match req.until.as_deref() {
        Some(s) if !s.trim().is_empty() => Some(parse_until(s)?),
        _ => None,
    };
    let changed = store::pause(&s.pool, req.id, until_utc)
        .await
        .map_err(|e| RemindersError::Other(e.to_string()))?;
    if !changed {
        return Err(RemindersError::NotFound(req.id));
    }
    fetch_view(&s.pool, req.id).await
}

async fn resume(
    State(s): State<Arc<RemindersState>>,
    Json(req): Json<IdReq>,
) -> Result<Json<ReminderView>, RemindersError> {
    let changed = store::resume(&s.pool, req.id)
        .await
        .map_err(|e| RemindersError::Other(e.to_string()))?;
    if !changed {
        return Err(RemindersError::NotFound(req.id));
    }
    fetch_view(&s.pool, req.id).await
}

async fn cancel(
    State(s): State<Arc<RemindersState>>,
    Json(req): Json<IdReq>,
) -> Result<Json<ReminderView>, RemindersError> {
    let changed = store::cancel(&s.pool, req.id)
        .await
        .map_err(|e| RemindersError::Other(e.to_string()))?;
    if !changed {
        return Err(RemindersError::NotFound(req.id));
    }
    fetch_view(&s.pool, req.id).await
}

async fn set_title(
    State(s): State<Arc<RemindersState>>,
    Json(req): Json<SetTitleReq>,
) -> Result<Json<ReminderView>, RemindersError> {
    let stored = req
        .title
        .as_deref()
        .filter(|t| !t.trim().is_empty());
    let rows = store::set_title(&s.pool, req.id, stored)
        .await
        .map_err(|e| RemindersError::Other(e.to_string()))?;
    if rows == 0 {
        return Err(RemindersError::NotFound(req.id));
    }
    fetch_view(&s.pool, req.id).await
}

async fn fetch_view(pool: &SqlitePool, id: i64) -> Result<Json<ReminderView>, RemindersError> {
    let inner = store::get(pool, id)
        .await
        .map_err(|e| RemindersError::Other(e.to_string()))?
        .ok_or(RemindersError::NotFound(id))?;
    let channels = store::channels_for(pool, id)
        .await
        .map_err(|e| RemindersError::Other(e.to_string()))?;
    Ok(Json(ReminderView { inner, channels }))
}

fn parse_until(s: &str) -> Result<DateTime<Utc>, RemindersError> {
    // Accept RFC3339 with offset, or a naive form treated as UTC.
    // Mirrors the leniency in store::pause's CLI counterpart.
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Ok(naive.and_utc());
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M") {
        return Ok(naive.and_utc());
    }
    Err(RemindersError::Other(format!(
        "unparseable `until`: {:?} (expected RFC3339 or YYYY-MM-DDTHH:MM)",
        s
    )))
}

// ─── errors ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum RemindersError {
    NotFound(i64),
    Other(String),
}

impl IntoResponse for RemindersError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::NotFound(id) => (
                StatusCode::NOT_FOUND,
                format!("reminder #{} not found (or already terminal)", id),
            ),
            Self::Other(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}
