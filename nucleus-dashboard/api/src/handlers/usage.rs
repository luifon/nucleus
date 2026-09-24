//! Usage surface (ADR-034) — token and estimated-cost accounting for every
//! Claude Code and Codex session on the machine.
//!
//! Read-only over `memory/usage.db`, whose single writer is
//! `nucleus usage refresh` (ADR-020 ownership). `POST /refresh` spawns that
//! command as a child process instead of writing from this process.
//!
//! Concurrency: at most one refresh child per server. A POST takes an
//! in-process gate, answers 409 when this server's child is still running
//! or another process holds the refresh lock (`flock` on
//! `memory/usage-refresh.lock`, e.g. the distiller), and only then spawns.
//! Simultaneous POSTs therefore start one child, not one each.
//!
//!   GET  /usage/api/status              data span, last refresh, prices
//!   POST /usage/api/refresh             start a refresh (202 / 409)
//!   GET  /usage/api/summary?days=N      KPIs, daily/weekly series, models, heatmap
//!   GET  /usage/api/projects?days=N     per-project totals
//!   GET  /usage/api/nucleus?days=N      Nucleus agents and reminders
//!   GET  /usage/api/limits?days=N       limit/error events, Codex quota by day
//!   GET  /usage/api/sessions?days=N&limit=M   largest sessions
//!
//! `days` = 0 means all time. Every data endpoint takes `vendor=all|claude|codex`
//! (default `all`); the filter is applied in SQL.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use nucleus_core::usage::{self, query};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;

pub struct UsageState {
    pub workspace_root: PathBuf,
    /// Registry agents launched on a schedule (`launch = "launchd-cron"`):
    /// the recurring jobs besides cron reminders.
    pub scheduled_agents: Vec<String>,
    pool: Mutex<Option<SqlitePool>>,
    /// Program and arguments of the refresh child. Default: this binary
    /// with `usage refresh`.
    refresh_cmd: (PathBuf, Vec<String>),
    /// Held while a POST checks and spawns (the single-flight gate).
    spawn_gate: Mutex<()>,
    /// True from spawn until this server's refresh child exits.
    child_running: Arc<AtomicBool>,
}

impl UsageState {
    pub fn new(workspace_root: PathBuf, scheduled_agents: Vec<String>) -> Self {
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("nucleus"));
        Self::with_refresh_command(workspace_root, scheduled_agents, exe, vec!["usage".into(), "refresh".into()])
    }

    /// Same as [`Self::new`] with an explicit refresh command (tests).
    pub fn with_refresh_command(
        workspace_root: PathBuf,
        scheduled_agents: Vec<String>,
        program: PathBuf,
        args: Vec<String>,
    ) -> Self {
        Self {
            workspace_root,
            scheduled_agents,
            pool: Mutex::new(None),
            refresh_cmd: (program, args),
            spawn_gate: Mutex::new(()),
            child_running: Arc::new(AtomicBool::new(false)),
        }
    }

    fn refreshing(&self) -> bool {
        self.child_running.load(Ordering::SeqCst) || usage::refresh_running(&self.workspace_root)
    }

    /// The read-only pool, opened on first use: the DB does not exist until
    /// the first refresh has run.
    async fn pool(&self) -> Result<Option<SqlitePool>, UsageError> {
        let mut guard = self.pool.lock().await;
        if guard.is_none() {
            *guard = usage::open_read_only(&self.workspace_root)
                .await
                .map_err(|e| UsageError::Internal(format!("{e:#}")))?;
        }
        Ok(guard.clone())
    }

    async fn require_pool(&self) -> Result<SqlitePool, UsageError> {
        self.pool().await?.ok_or(UsageError::NoData)
    }
}

pub fn router(state: Arc<UsageState>) -> Router {
    Router::new()
        .route("/status", get(status))
        .route("/refresh", post(refresh))
        .route("/summary", get(summary))
        .route("/projects", get(projects))
        .route("/nucleus", get(nucleus))
        .route("/limits", get(limits))
        .route("/sessions", get(sessions))
        .with_state(state)
}

#[derive(Deserialize)]
struct RangeQ {
    #[serde(default = "default_days")]
    days: u32,
    limit: Option<u32>,
    /// Tool filter: `all` (default), `claude`, `codex`.
    #[serde(default)]
    vendor: query::VendorFilter,
}

fn default_days() -> u32 {
    30
}

pub enum UsageError {
    NoData,
    Busy,
    Internal(String),
}

impl IntoResponse for UsageError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::NoData => (
                StatusCode::SERVICE_UNAVAILABLE,
                "no usage data yet — start a refresh".to_string(),
            ),
            Self::Busy => (StatusCode::CONFLICT, "a usage refresh is already running".to_string()),
            Self::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

fn internal(e: anyhow::Error) -> UsageError {
    UsageError::Internal(format!("{e:#}"))
}

async fn status(State(s): State<Arc<UsageState>>) -> Result<Json<query::UsageStatus>, UsageError> {
    let pool = s.pool().await?;
    let mut st = query::status(pool.as_ref(), &s.workspace_root).await.map_err(internal)?;
    // Covers the moments between spawn and the child taking the lock.
    st.refreshing = st.refreshing || s.child_running.load(Ordering::SeqCst);
    Ok(Json(st))
}

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct RefreshStarted {
    started: bool,
    #[ts(type = "number | null")]
    pid: Option<u32>,
}

/// Spawn `nucleus usage refresh` from the same binary this server runs as.
/// Output goes to `memory/usage-refresh.log`.
async fn refresh(State(s): State<Arc<UsageState>>) -> Result<(StatusCode, Json<RefreshStarted>), UsageError> {
    // Single flight: a concurrent POST finds the gate taken and gets 409
    // instead of spawning a second child.
    let Ok(_gate) = s.spawn_gate.try_lock() else {
        return Err(UsageError::Busy);
    };
    if s.refreshing() {
        return Err(UsageError::Busy);
    }
    let log_path = s.workspace_root.join("memory/usage-refresh.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| UsageError::Internal(format!("{}: {e}", log_path.display())))?;
    let err_log = log.try_clone().map_err(|e| UsageError::Internal(e.to_string()))?;
    let (program, args) = &s.refresh_cmd;
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .current_dir(&s.workspace_root)
        .stdin(std::process::Stdio::null())
        .stdout(log)
        .stderr(err_log)
        .spawn()
        .map_err(|e| UsageError::Internal(format!("spawning usage refresh: {e}")))?;
    let pid = child.id();
    s.child_running.store(true, Ordering::SeqCst);
    let running = s.child_running.clone();
    // Reap the child and clear the flag when it exits.
    tokio::spawn(async move {
        let _ = child.wait().await;
        running.store(false, Ordering::SeqCst);
    });
    Ok((StatusCode::ACCEPTED, Json(RefreshStarted { started: true, pid })))
}

async fn summary(State(s): State<Arc<UsageState>>, Query(q): Query<RangeQ>) -> Result<Json<query::UsageSummary>, UsageError> {
    let pool = s.require_pool().await?;
    Ok(Json(query::summary(&pool, q.days, q.vendor).await.map_err(internal)?))
}

async fn projects(
    State(s): State<Arc<UsageState>>,
    Query(q): Query<RangeQ>,
) -> Result<Json<Vec<query::UsageProjectRow>>, UsageError> {
    let pool = s.require_pool().await?;
    Ok(Json(query::projects(&pool, q.days, q.vendor).await.map_err(internal)?))
}

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct UsageNucleusView {
    usage: query::UsageNucleus,
    /// Agents the registry launches on a schedule; with cron reminders,
    /// these are the recurring jobs.
    scheduled_agents: Vec<String>,
    /// The workspace's project name. Its sessions without a label are
    /// grouped as `unlabeled` (see `query::nucleus`).
    workspace_project: String,
}

async fn nucleus(State(s): State<Arc<UsageState>>, Query(q): Query<RangeQ>) -> Result<Json<UsageNucleusView>, UsageError> {
    let pool = s.require_pool().await?;
    let ws = s.workspace_root.to_string_lossy().into_owned();
    Ok(Json(UsageNucleusView {
        usage: query::nucleus(&pool, q.days, &ws, q.vendor).await.map_err(internal)?,
        scheduled_agents: s.scheduled_agents.clone(),
        workspace_project: s
            .workspace_root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    }))
}

async fn limits(State(s): State<Arc<UsageState>>, Query(q): Query<RangeQ>) -> Result<Json<query::UsageLimits>, UsageError> {
    let pool = s.require_pool().await?;
    Ok(Json(query::limits(&pool, q.days, q.vendor).await.map_err(internal)?))
}

async fn sessions(
    State(s): State<Arc<UsageState>>,
    Query(q): Query<RangeQ>,
) -> Result<Json<Vec<query::UsageSessionRow>>, UsageError> {
    let pool = s.require_pool().await?;
    let limit = q.limit.unwrap_or(25).clamp(1, 200);
    Ok(Json(query::sessions(&pool, q.days, limit, q.vendor).await.map_err(internal)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn post() -> Request<Body> {
        Request::builder().method("POST").uri("/refresh").body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn simultaneous_refresh_posts_spawn_one_child() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        let state = Arc::new(UsageState::with_refresh_command(
            dir.path().to_path_buf(),
            vec![],
            PathBuf::from("/bin/sleep"),
            vec!["2".into()],
        ));
        let app = router(state.clone());
        let calls = (0..16).map(|_| app.clone().oneshot(post()));
        let codes: Vec<StatusCode> = futures::future::join_all(calls).await.into_iter().map(|r| r.unwrap().status()).collect();
        assert_eq!(codes.iter().filter(|c| **c == StatusCode::ACCEPTED).count(), 1, "{codes:?}");
        assert_eq!(codes.iter().filter(|c| **c == StatusCode::CONFLICT).count(), 15);
        // While the child runs, a later POST is refused as well.
        assert_eq!(app.clone().oneshot(post()).await.unwrap().status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn refresh_is_refused_while_another_process_holds_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let _lock = usage::RefreshLock::acquire(dir.path()).await.unwrap();
        let state = Arc::new(UsageState::with_refresh_command(
            dir.path().to_path_buf(),
            vec![],
            PathBuf::from("/bin/sleep"),
            vec!["0".into()],
        ));
        let res = router(state).oneshot(post()).await.unwrap();
        assert_eq!(res.status(), StatusCode::CONFLICT);
    }
}
