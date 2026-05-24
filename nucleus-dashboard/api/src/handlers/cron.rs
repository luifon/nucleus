//! Cron surface — aggregated view of scheduled work.
//!
//! Three data sources stitched together:
//!   - **launchd**: `launchctl list | grep dev.nucleus` for currently-
//!     loaded plists (label + PID + last exit status).
//!   - **upcoming fires**: `reminders` table sorted by `next_fire_at`.
//!   - **recent fires**: `reminder_fires` audit log for the last N.
//!
//! Read-only. Pause/resume/cancel belong on the dedicated `/reminders`
//! surface (lands separately). Modifying launchd plists is never a
//! Console action.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use serde::Serialize;
use sqlx::SqlitePool;
use std::sync::Arc;
use tokio::process::Command;

#[derive(Clone)]
pub struct CronState {
    pub reminders_pool: Option<SqlitePool>,
}

pub fn router(state: Arc<CronState>) -> Router {
    Router::new()
        .route("/launchd", get(list_launchd))
        .route("/upcoming", get(list_upcoming))
        .route("/recent", get(list_recent))
        .with_state(state)
}

// ─── launchd ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct LaunchdJob {
    label: String,
    /// PID if the job is currently running; otherwise None (cron-style
    /// jobs that just ran their command and exited.)
    pid: Option<i32>,
    /// Last exit status. 0 = clean. Non-zero = error. Negative on macOS
    /// generally means signal-killed (e.g. -9 = SIGKILL, -15 = SIGTERM).
    last_exit: Option<i32>,
}

async fn list_launchd() -> Result<Json<Vec<LaunchdJob>>, CronError> {
    // `launchctl list` prints `PID\tStatus\tLabel` rows for every loaded
    // job in the user's GUI domain. Grep is faster than asking launchctl
    // to filter, and we only want our prefix.
    let output = Command::new("launchctl")
        .arg("list")
        .output()
        .await
        .map_err(|e| CronError::Spawn(e.to_string()))?;
    if !output.status.success() {
        return Err(CronError::LaunchctlFailed(
            String::from_utf8_lossy(&output.stderr).to_string(),
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);

    let mut jobs = Vec::new();
    for line in text.lines() {
        // First line is the header `PID\tStatus\tLabel`. Skip.
        if line.starts_with("PID") {
            continue;
        }
        let mut parts = line.split('\t');
        let pid_field = parts.next().unwrap_or("");
        let status_field = parts.next().unwrap_or("");
        let label = match parts.next() {
            Some(s) => s.trim().to_string(),
            None => continue,
        };
        if !label.starts_with("dev.nucleus.") {
            continue;
        }
        let pid = pid_field.parse::<i32>().ok();
        let last_exit = status_field.parse::<i32>().ok();
        jobs.push(LaunchdJob { label, pid, last_exit });
    }
    // Stable alphabetical order so the operator's eye doesn't have to
    // chase rows around on every reload.
    jobs.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(Json(jobs))
}

// ─── upcoming reminders ─────────────────────────────────────────────────────

#[derive(Serialize, sqlx::FromRow)]
struct UpcomingFire {
    id: i64,
    /// Operator-set short name (ADR-015). When null the frontend
    /// derives a label from body or system_prompt.
    title: Option<String>,
    body: Option<String>,
    cron: Option<String>,
    one_shot: i64,
    status: String,
    next_fire_at: String,
    last_fired_at: Option<String>,
    created_by: String,
    /// system_prompt is the ADR-008 skill-fire field. Surface its
    /// presence so the operator knows "this fire spawns a Claude
    /// session" vs "this fire posts a static body".
    system_prompt: Option<String>,
    /// Aggregated channel list — single string joined with ` | ` so
    /// the row stays one render.
    channels: Option<String>,
}

async fn list_upcoming(State(s): State<Arc<CronState>>) -> Result<Json<Vec<UpcomingFire>>, CronError> {
    let pool = s.reminders_pool.as_ref().ok_or(CronError::NoReminders)?;
    let rows: Vec<UpcomingFire> = sqlx::query_as::<_, UpcomingFire>(
        r#"
        SELECT r.id, r.title, r.body, r.cron, r.one_shot, r.status,
               r.next_fire_at, r.last_fired_at, r.created_by,
               r.system_prompt,
               (SELECT GROUP_CONCAT(channel, ' | ')
                  FROM reminder_channels c WHERE c.reminder_id = r.id) AS channels
        FROM reminders r
        WHERE r.status IN ('active', 'pending')
          AND r.next_fire_at IS NOT NULL
        ORDER BY r.next_fire_at ASC
        LIMIT 40
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(Json(rows))
}

// ─── recent fires ───────────────────────────────────────────────────────────

#[derive(Serialize, sqlx::FromRow)]
struct RecentFire {
    id: i64,
    reminder_id: i64,
    fired_at: String,
    channel: String,
    success: i64,
    msg_id: Option<String>,
    error: Option<String>,
    /// Operator-set short name (ADR-015). Joined in alongside body
    /// so the frontend can pick whichever is set.
    reminder_title: Option<String>,
    /// Reminder body for context — joined in at query time so the
    /// frontend doesn't have to N+1 fetch.
    reminder_body: Option<String>,
    /// Whether the reminder fires a Claude session (ADR-008 skill-fire)
    /// vs a static body. Surfaced as a small icon on the row.
    is_skill_fire: i64,
}

async fn list_recent(State(s): State<Arc<CronState>>) -> Result<Json<Vec<RecentFire>>, CronError> {
    let pool = s.reminders_pool.as_ref().ok_or(CronError::NoReminders)?;
    let rows: Vec<RecentFire> = sqlx::query_as::<_, RecentFire>(
        r#"
        SELECT f.id, f.reminder_id, f.fired_at, f.channel, f.success,
               f.msg_id, f.error,
               r.title AS reminder_title,
               r.body AS reminder_body,
               CASE WHEN r.system_prompt IS NOT NULL AND r.system_prompt != ''
                    THEN 1 ELSE 0 END AS is_skill_fire
        FROM reminder_fires f
        LEFT JOIN reminders r ON r.id = f.reminder_id
        ORDER BY f.fired_at DESC
        LIMIT 60
        "#,
    )
    .fetch_all(pool)
    .await?;
    Ok(Json(rows))
}

// ─── errors ─────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum CronError {
    Sqlx(sqlx::Error),
    NoReminders,
    Spawn(String),
    LaunchctlFailed(String),
}

impl From<sqlx::Error> for CronError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sqlx(e)
    }
}

impl IntoResponse for CronError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::Sqlx(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {}", e)),
            Self::NoReminders => (
                StatusCode::SERVICE_UNAVAILABLE,
                "reminders.db not openable — reminders subsystem may not be initialized yet".into(),
            ),
            Self::Spawn(m) => (StatusCode::INTERNAL_SERVER_ERROR, format!("spawn: {}", m)),
            Self::LaunchctlFailed(m) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("launchctl: {}", m),
            ),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}
