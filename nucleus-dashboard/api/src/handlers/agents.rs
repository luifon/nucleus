//! Agents surface — the operator front door (ADR-016).
//!
//! Reads the agent registry (`agents.toml`) and, per agent, computes live
//! state by probing the runtime its `launch` implies:
//!   - launchd-daemon → PID present in `launchctl list` = running
//!   - launchd-cron   → last exit code (0 = idle-ok, nonzero = errored;
//!                      PID present = mid-run)
//!   - in-process     → hosted by the dashboard (which is obviously up, since
//!                      it's answering this request)
//!   - on-demand      → live tmux window present = running, else idle
//!
//! It also resolves each conversational agent's persona display name (ADR-009)
//! and exposes the per-agent run-log index (transcript pointers, ADR-016).
//!
//! Read-only. The registry is loaded once at startup (hand-edited; restart to
//! pick up edits). This is the surface `/sessions` collapsed into — the only
//! tmux affordance kept is the copy-attach command, surfaced per agent tile.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use nucleus_core::agents::{Agent, Registry};
use nucleus_core::config::Identity;
use nucleus_core::runlog;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::process::Command;

#[derive(Clone)]
pub struct AgentsState {
    pub workspace_root: PathBuf,
    pub registry: Registry,
    /// For resolving persona display names (ADR-009).
    pub identity: Identity,
}

pub fn router(state: Arc<AgentsState>) -> Router {
    Router::new()
        .route("/list", get(list_agents))
        .route("/runs", get(list_runs))
        .route("/log", get(get_log))
        .with_state(state)
}

// ─── list ─────────────────────────────────────────────────────────────────

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct AgentView {
    name: String,
    class: nucleus_core::agents::AgentClass,
    launch: nucleus_core::agents::Launch,
    runtime: Option<String>,
    schedule: Option<String>,
    diary_key: Option<String>,
    persona_venue: Option<String>,
    /// Resolved persona name (ADR-009) for conversational agents, if any.
    persona_display_name: Option<String>,
    capabilities: Vec<nucleus_core::agents::Capability>,
    tmux_session: Option<String>,
    launchd_label: Option<String>,

    // ── computed liveness ──
    /// running | idle | errored | hosted | stopped | unknown
    status: &'static str,
    /// PID for launchd jobs currently running.
    pid: Option<i32>,
    /// Last exit code for launchd-cron jobs (0 clean; negative = signal).
    last_exit: Option<i32>,
    /// Live tmux windows matching this agent's session (prefix match).
    live_windows: usize,
    /// Most recent tmux window activity (epoch secs), if any live window.
    #[ts(type = "number | null")]
    last_activity_unix: Option<i64>,
    /// `started_at` of the most recent run-log row (tmux agents).
    last_run_started: Option<String>,
    /// Count of indexed runs (tmux agents).
    run_count: usize,
    /// `tmux attach -t <session>` convenience for the live ones.
    attach_cmd: Option<String>,
}

/// One `launchctl list` row we care about.
struct LaunchdState {
    pid: Option<i32>,
    last_exit: Option<i32>,
}

async fn list_agents(State(s): State<Arc<AgentsState>>) -> Result<Json<Vec<AgentView>>, AgentsError> {
    let launchd = probe_launchd().await;
    let tmux = probe_tmux().await;

    let mut out = Vec::new();
    for agent in s.registry.enabled() {
        out.push(view_for(agent, &s, &launchd, &tmux));
    }
    Ok(Json(out))
}

fn view_for(
    agent: &Agent,
    s: &Arc<AgentsState>,
    launchd: &HashMap<String, LaunchdState>,
    tmux: &HashMap<String, (i64, usize)>,
) -> AgentView {
    use nucleus_core::agents::Launch;

    let ld = agent
        .launchd_label
        .as_deref()
        .and_then(|l| launchd.get(l));
    let pid = ld.and_then(|s| s.pid);
    let last_exit = ld.and_then(|s| s.last_exit);

    // Live tmux windows: prefix match so e.g. `nucleus-whatsapp` also counts
    // `nucleus-whatsapp-dm` / `-braindump`.
    let (live_windows, last_activity_unix) = match &agent.tmux_session {
        Some(prefix) => {
            let mut windows = 0usize;
            let mut activity: Option<i64> = None;
            for (name, (act, wins)) in tmux {
                if name == prefix || name.starts_with(&format!("{prefix}-")) {
                    windows += wins;
                    activity = Some(activity.map_or(*act, |a| a.max(*act)));
                }
            }
            (windows, activity)
        }
        None => (0, None),
    };

    let status: &'static str = match agent.launch {
        Launch::LaunchdDaemon => {
            if pid.is_some() {
                "running"
            } else {
                "stopped"
            }
        }
        Launch::LaunchdCron => {
            if pid.is_some() {
                "running"
            } else {
                match last_exit {
                    Some(0) => "idle",
                    Some(_) => "errored",
                    None => "unknown",
                }
            }
        }
        // The dashboard is serving this request, so an in-process agent
        // (the chat pool) is up by construction.
        Launch::InProcess => "hosted",
        Launch::OnDemand => {
            if live_windows > 0 {
                "running"
            } else {
                "idle"
            }
        }
    };

    let persona_display_name = agent.persona_venue.as_deref().and_then(|venue| {
        nucleus_core::config::resolve_persona(&s.identity, venue, None)
            .ok()
            .map(|p| p.display_name)
    });

    let runs = if agent.is_claude_tmux() {
        runlog::read(&s.workspace_root, &agent.name)
    } else {
        Vec::new()
    };
    let last_run_started = runs.last().map(|r| r.started_at.clone());

    let attach_cmd = match &agent.tmux_session {
        Some(sess) if live_windows > 0 => Some(format!("tmux attach -t {sess}")),
        _ => None,
    };

    AgentView {
        name: agent.name.clone(),
        class: agent.class,
        launch: agent.launch,
        runtime: agent.runtime.clone(),
        schedule: agent.schedule.clone(),
        diary_key: agent.diary_key.clone(),
        persona_venue: agent.persona_venue.clone(),
        persona_display_name,
        capabilities: agent.capabilities.clone(),
        tmux_session: agent.tmux_session.clone(),
        launchd_label: agent.launchd_label.clone(),
        status,
        pid,
        last_exit,
        live_windows,
        last_activity_unix,
        last_run_started,
        run_count: runs.len(),
        attach_cmd,
    }
}

// ─── runs (transcript index) ────────────────────────────────────────────────

#[derive(Deserialize)]
struct RunsQuery {
    agent: String,
}

async fn list_runs(
    State(s): State<Arc<AgentsState>>,
    Query(q): Query<RunsQuery>,
) -> Result<Json<Vec<runlog::RunRow>>, AgentsError> {
    if s.registry.get(&q.agent).is_none() {
        return Err(AgentsError::UnknownAgent(q.agent));
    }
    // Newest first — the operator's eye lands on the latest run.
    let mut rows = runlog::read(&s.workspace_root, &q.agent);
    rows.reverse();
    Ok(Json(rows))
}

// ─── log (launchd stdout/err tail) ───────────────────────────────────────────

#[derive(Deserialize)]
struct LogQuery {
    agent: String,
}

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct LogResponse {
    agent: String,
    path: String,
    /// Last ~200 lines of the launchd log, oldest-first.
    tail: String,
}

const LOG_TAIL_LINES: usize = 200;

async fn get_log(
    State(s): State<Arc<AgentsState>>,
    Query(q): Query<LogQuery>,
) -> Result<Json<LogResponse>, AgentsError> {
    let agent = s
        .registry
        .get(&q.agent)
        .ok_or_else(|| AgentsError::UnknownAgent(q.agent.clone()))?;
    let rel = agent.log_path.as_deref().ok_or(AgentsError::NoLog)?;
    let path = s.workspace_root.join(rel);
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| AgentsError::Io(e.to_string()))?;
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.len().saturating_sub(LOG_TAIL_LINES);
    let tail = lines[start..].join("\n");
    Ok(Json(LogResponse {
        agent: q.agent,
        path: rel.to_string(),
        tail,
    }))
}

// ─── probes ──────────────────────────────────────────────────────────────────

/// `launchctl list` → label → {pid, last_exit} for `dev.nucleus.*`.
/// Mirrors the parse in `cron.rs`; kept local so `/agents` owns its own
/// liveness computation rather than coupling to the cron surface.
async fn probe_launchd() -> HashMap<String, LaunchdState> {
    let mut map = HashMap::new();
    let Ok(out) = Command::new("launchctl").arg("list").output().await else {
        return map;
    };
    if !out.status.success() {
        return map;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if line.starts_with("PID") {
            continue;
        }
        let mut parts = line.split('\t');
        let pid_field = parts.next().unwrap_or("");
        let status_field = parts.next().unwrap_or("");
        let Some(label) = parts.next().map(|s| s.trim().to_string()) else {
            continue;
        };
        if !label.starts_with("dev.nucleus.") {
            continue;
        }
        map.insert(
            label,
            LaunchdState {
                pid: pid_field.parse().ok(),
                last_exit: status_field.parse().ok(),
            },
        );
    }
    map
}

/// `tmux list-sessions` → session name → (last activity unix, window count)
/// for `nucleus-*`. Mirrors the parse in `sessions.rs`. Empty when no server.
async fn probe_tmux() -> HashMap<String, (i64, usize)> {
    let mut map = HashMap::new();
    let Ok(out) = Command::new("tmux")
        .args([
            "list-sessions",
            "-F",
            "#{session_name}\t#{session_activity}\t#{session_windows}",
        ])
        .output()
        .await
    else {
        return map;
    };
    if !out.status.success() {
        return map; // "no server running" → no live sessions
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.split('\t');
        let name = parts.next().unwrap_or("").to_string();
        if !name.starts_with("nucleus-") {
            continue;
        }
        let activity = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let windows = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        map.insert(name, (activity, windows));
    }
    map
}

// ─── errors ──────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum AgentsError {
    UnknownAgent(String),
    NoLog,
    Io(String),
}

impl IntoResponse for AgentsError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::UnknownAgent(a) => (StatusCode::NOT_FOUND, format!("unknown agent: {a}")),
            Self::NoLog => (
                StatusCode::NOT_FOUND,
                "agent has no log_path (tmux agent — use /runs for its transcript index)".into(),
            ),
            Self::Io(m) => (StatusCode::INTERNAL_SERVER_ERROR, format!("reading log: {m}")),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}
