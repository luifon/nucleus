//! Diary surface — per-agent dated entries from `memory/diaries/`.
//!
//! Per ADR-004, every bot/worker writes a chronological diary as
//! `memory/diaries/<agent>/<YYYY-MM-DD>.md`. Each file is a sequence
//! of `## HH:MM — <activity>` sections with structured observations.
//!
//! Three endpoints:
//!   - `/diary/api/agents`        list of agents with entry counts +
//!                                most-recent-entry date
//!   - `/diary/api/recent`        flattened most-recent entries across
//!                                all agents (or one if `?agent=X`),
//!                                bodies inlined for one-trip render
//!   - `/diary/api/entry`         single full entry by path (used by
//!                                the frontend if an entry was
//!                                truncated in `/recent`)
//!
//! Read-only. Path traversal on the entry endpoint is guarded by
//! canonicalizing against the diary root.

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Clone)]
pub struct DiaryState {
    pub root: PathBuf,
}

pub fn router(state: Arc<DiaryState>) -> Router {
    Router::new()
        .route("/agents", get(list_agents))
        .route("/recent", get(list_recent))
        .route("/entry", get(get_entry))
        .with_state(state)
}

// ─── agents ─────────────────────────────────────────────────────────────────

#[derive(Serialize)]
struct Agent {
    name: String,
    entry_count: usize,
    /// Most recent date that has an entry, in `YYYY-MM-DD` (sorted
    /// lexicographically — diary filenames already match that
    /// format, no parse).
    last_entry_date: Option<String>,
}

async fn list_agents(State(s): State<Arc<DiaryState>>) -> Result<Json<Vec<Agent>>, DiaryError> {
    let mut agents = Vec::new();
    let mut entries = match tokio::fs::read_dir(&s.root).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Json(agents)),
        Err(e) => return Err(DiaryError::Io(e.to_string())),
    };
    while let Some(dirent) = entries
        .next_entry()
        .await
        .map_err(|e| DiaryError::Io(e.to_string()))?
    {
        let path = dirent.path();
        let file_type = match dirent.file_type().await {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) if !n.starts_with('.') => n.to_string(),
            _ => continue,
        };
        let dates = collect_dates(&path).await?;
        let last = dates.last().cloned();
        agents.push(Agent {
            name,
            entry_count: dates.len(),
            last_entry_date: last,
        });
    }
    // Most-recently-active agents first; ties broken by name so the
    // order stays stable across reloads.
    agents.sort_by(|a, b| {
        b.last_entry_date
            .cmp(&a.last_entry_date)
            .then_with(|| a.name.cmp(&b.name))
    });
    Ok(Json(agents))
}

/// Returns dated entry filenames (without `.md`) for one agent, sorted
/// ascending. Files starting with `_` (e.g. `_pending.md` — in-flight
/// scratch state, ADR-004) are skipped — they're not user-facing entries.
async fn collect_dates(agent_dir: &Path) -> Result<Vec<String>, DiaryError> {
    let mut dates = Vec::new();
    let mut entries = tokio::fs::read_dir(agent_dir)
        .await
        .map_err(|e| DiaryError::Io(e.to_string()))?;
    while let Some(dirent) = entries
        .next_entry()
        .await
        .map_err(|e| DiaryError::Io(e.to_string()))?
    {
        let name = match dirent.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let Some(stem) = name.strip_suffix(".md") else { continue };
        if stem.starts_with('_') {
            continue;
        }
        dates.push(stem.to_string());
    }
    dates.sort();
    Ok(dates)
}

// ─── recent ────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct RecentQ {
    /// Restrict to one agent. When omitted, returns most-recent
    /// entries across all agents.
    agent: Option<String>,
    /// Restrict to one date (`YYYY-MM-DD`). When set, returns every
    /// matching entry for that day (one per agent at most) and
    /// `limit` is ignored — a single day is small.
    date: Option<String>,
    /// Total entries to return when `date` is unset. Defaults to 20,
    /// clamped to [1, 100].
    limit: Option<usize>,
}

#[derive(Serialize)]
struct Entry {
    agent: String,
    date: String,
    /// Absolute path. The frontend uses this to re-fetch the entry
    /// individually if the operator drills in.
    path: String,
    body: String,
    /// Size of the body in bytes — useful when surfacing "abnormally
    /// large" entries (e.g., a bot stuck in a loop).
    bytes: usize,
}

async fn list_recent(
    State(s): State<Arc<DiaryState>>,
    Query(q): Query<RecentQ>,
) -> Result<Json<Vec<Entry>>, DiaryError> {
    let limit = q.limit.unwrap_or(20).clamp(1, 100);

    // Build (agent, date, path) tuples for every entry we might
    // return. For per-agent queries we only walk one folder; otherwise
    // we walk every folder.
    let mut candidates: Vec<(String, String, PathBuf)> = Vec::new();
    if let Some(agent) = q.agent.as_ref() {
        let agent_dir = s.root.join(agent);
        if tokio::fs::try_exists(&agent_dir).await.unwrap_or(false) {
            for d in collect_dates(&agent_dir).await? {
                candidates.push((agent.clone(), d.clone(), agent_dir.join(format!("{d}.md"))));
            }
        }
    } else {
        let mut entries = match tokio::fs::read_dir(&s.root).await {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Json(Vec::new())),
            Err(e) => return Err(DiaryError::Io(e.to_string())),
        };
        while let Some(dirent) = entries
            .next_entry()
            .await
            .map_err(|e| DiaryError::Io(e.to_string()))?
        {
            let agent_dir = dirent.path();
            let file_type = match dirent.file_type().await {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if !file_type.is_dir() {
                continue;
            }
            let agent_name = match agent_dir.file_name().and_then(|n| n.to_str()) {
                Some(n) if !n.starts_with('.') => n.to_string(),
                _ => continue,
            };
            for d in collect_dates(&agent_dir).await? {
                candidates.push((agent_name.clone(), d.clone(), agent_dir.join(format!("{d}.md"))));
            }
        }
    }

    // Date filter (server-side) — when set, narrow to that exact date
    // and skip the limit truncation since a single day is bounded by
    // the number of agents.
    if let Some(date) = q.date.as_ref() {
        candidates.retain(|(_, d, _)| d == date);
    }

    // Sort by date DESC (lex sort on ISO dates is correct), tie-break
    // by agent name so the order is deterministic when multiple agents
    // wrote on the same day.
    candidates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    if q.date.is_none() {
        candidates.truncate(limit);
    }

    let mut out = Vec::with_capacity(candidates.len());
    for (agent, date, path) in candidates {
        let body = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| DiaryError::Io(format!("reading {}: {}", path.display(), e)))?;
        let bytes = body.len();
        out.push(Entry {
            agent,
            date,
            path: path.to_string_lossy().into_owned(),
            body,
            bytes,
        });
    }
    Ok(Json(out))
}

// ─── single entry ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct EntryQ {
    path: String,
}

async fn get_entry(
    State(s): State<Arc<DiaryState>>,
    Query(q): Query<EntryQ>,
) -> Result<String, DiaryError> {
    let requested = PathBuf::from(&q.path);
    let canonical = tokio::fs::canonicalize(&requested)
        .await
        .map_err(|e| DiaryError::Io(format!("canonicalizing {}: {}", q.path, e)))?;
    let canon_root = tokio::fs::canonicalize(&s.root)
        .await
        .map_err(|e| DiaryError::Io(format!("canonicalizing root: {}", e)))?;
    if !canonical.starts_with(&canon_root) {
        return Err(DiaryError::OutsideRoot);
    }
    if canonical.extension().and_then(|e| e.to_str()) != Some("md") {
        return Err(DiaryError::OutsideRoot);
    }
    tokio::fs::read_to_string(&canonical)
        .await
        .map_err(|e| DiaryError::Io(format!("reading {}: {}", canonical.display(), e)))
}

// ─── errors ─────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum DiaryError {
    Io(String),
    OutsideRoot,
}

impl IntoResponse for DiaryError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::Io(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
            Self::OutsideRoot => (
                StatusCode::FORBIDDEN,
                "path is not inside the diary root".to_string(),
            ),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}
