//! Sessions surface — tmux inspector for the long-lived
//! `nucleus-*` sessions hosting the bot Claude sessions (Rule 4).
//!
//! Read-only. The dashboard never attaches, sends keys, or kills
//! tmux sessions — those are operator-only actions via a terminal.
//! The frontend offers a copy-to-clipboard for the `tmux attach`
//! command instead.
//!
//! One endpoint:
//!   - `GET /sessions/api/list` — every `nucleus-*` session with its
//!     window list inlined. Responses are tiny in practice (8-ish
//!     sessions × 1 window each); no need for a second hop.

use axum::{
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use serde::Serialize;
use tokio::process::Command;

pub fn router() -> Router {
    Router::new().route("/list", get(list_sessions))
}

#[derive(Serialize)]
struct TmuxWindow {
    /// Index within its parent session. Stable until a window is killed.
    index: i32,
    name: String,
    /// Unix epoch seconds of the window's last activity (output / keypress).
    activity_unix: i64,
    panes: i32,
}

#[derive(Serialize)]
struct TmuxSession {
    name: String,
    /// Unix epoch seconds. Useful when correlating with the daily
    /// 04:00 rotation — sessions created near 04:00 today have just
    /// rotated.
    created_unix: i64,
    activity_unix: i64,
    /// 1 if a client is currently attached (tmux attach), 0 otherwise.
    attached: i32,
    windows: Vec<TmuxWindow>,
}

async fn list_sessions() -> Result<Json<Vec<TmuxSession>>, SessionsError> {
    // Single tmux call for sessions, with a custom format. Each field
    // separated by a tab so we don't have to worry about spaces in
    // session names.
    let sess_out = Command::new("tmux")
        .args([
            "list-sessions",
            "-F",
            "#{session_name}\t#{session_created}\t#{session_activity}\t#{session_attached}",
        ])
        .output()
        .await
        .map_err(|e| SessionsError::Spawn(e.to_string()))?;

    // tmux exits 1 ("no server running") when there are no sessions —
    // treat as an empty list rather than an error so the page renders
    // cleanly on a fresh boot.
    if !sess_out.status.success() {
        let stderr = String::from_utf8_lossy(&sess_out.stderr);
        if stderr.contains("no server running") || stderr.contains("no sessions") {
            return Ok(Json(Vec::new()));
        }
        return Err(SessionsError::TmuxFailed(stderr.into_owned()));
    }

    let mut sessions = Vec::new();
    for line in String::from_utf8_lossy(&sess_out.stdout).lines() {
        let mut parts = line.split('\t');
        let name = parts.next().unwrap_or("").to_string();
        if !name.starts_with("nucleus-") {
            continue;
        }
        let created_unix = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let activity_unix = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let attached = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);

        // Per-session windows. One call per session. With ~10 sessions
        // and 1-2 windows each this stays well under the human-perceptible
        // threshold even on a cold tmux server.
        let win_out = Command::new("tmux")
            .args([
                "list-windows",
                "-t",
                &name,
                "-F",
                "#{window_index}\t#{window_name}\t#{window_activity}\t#{window_panes}",
            ])
            .output()
            .await
            .map_err(|e| SessionsError::Spawn(e.to_string()))?;
        let mut windows = Vec::new();
        if win_out.status.success() {
            for wline in String::from_utf8_lossy(&win_out.stdout).lines() {
                let mut wp = wline.split('\t');
                let index = wp.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                let win_name = wp.next().unwrap_or("").to_string();
                let win_activity = wp.next().and_then(|s| s.parse().ok()).unwrap_or(0);
                let panes = wp.next().and_then(|s| s.parse().ok()).unwrap_or(1);
                windows.push(TmuxWindow {
                    index,
                    name: win_name,
                    activity_unix: win_activity,
                    panes,
                });
            }
        }

        sessions.push(TmuxSession {
            name,
            created_unix,
            activity_unix,
            attached,
            windows,
        });
    }

    // Most-recently-active first so the operator's eye lands on what
    // moved last.
    sessions.sort_by(|a, b| b.activity_unix.cmp(&a.activity_unix));
    Ok(Json(sessions))
}

#[derive(Debug)]
pub enum SessionsError {
    Spawn(String),
    TmuxFailed(String),
}

impl IntoResponse for SessionsError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::Spawn(m) => (StatusCode::INTERNAL_SERVER_ERROR, format!("spawn tmux: {}", m)),
            Self::TmuxFailed(m) => (StatusCode::INTERNAL_SERVER_ERROR, format!("tmux: {}", m)),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}
