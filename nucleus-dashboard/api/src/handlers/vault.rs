//! Vault surface — chronological feed of writes into the Obsidian
//! vault (per ADR-005). Backed by filesystem mtime — no audit log
//! exists for brain-dump applies, so the feed reflects "what files
//! changed recently" rather than "what the apply pipeline did".
//! Good enough for the operator's "what did the bot write?" question;
//! see ADR-015 §"Scope" for the audit-log alternative.
//!
//! ADR-035 adds full-text search (`/search`, through core's vault index —
//! the same code and DB the `vault-search` CLI uses) and the weekly vault
//! check report (`/check/latest`, `/check/runs`, read-only from
//! `memory/vault_check.db`, which only `vault-check` writes).

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use nucleus_core::vault::check::{self, CheckReport, CheckRunSummary};
use nucleus_core::vault::exclude::Exclusions;
use nucleus_core::vault::index::{self, VaultSearchResult};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

pub struct VaultState {
    pub root: PathBuf,
    /// Core's vault index (ADR-035). `None` when it could not be opened or
    /// the exclusion config is invalid; `/search` then answers 503.
    pub search: Option<VaultSearch>,
    /// `memory/vault_check.db`, opened read-only per request.
    pub check_db: PathBuf,
}

pub struct VaultSearch {
    pub pool: SqlitePool,
    pub exclusions: Exclusions,
    /// One update at a time from this process; other processes serialize
    /// on SQLite's write lock inside `index::update`.
    pub update_lock: tokio::sync::Mutex<()>,
}

pub fn router(state: Arc<VaultState>) -> Router {
    Router::new()
        .route("/recent", get(list_recent))
        .route("/file", get(get_file))
        .route("/buckets", get(list_buckets))
        .route("/search", get(search))
        .route("/check/latest", get(check_latest))
        .route("/check/runs", get(check_runs))
        .with_state(state)
}

// ─── search (ADR-035) ───────────────────────────────────────────────────────

#[derive(Deserialize)]
struct SearchQ {
    q: String,
    /// Bucket or folder path prefix (`3-Projects`, `4-Areas/Health`).
    bucket: Option<String>,
    /// Defaults to 20, clamped to [1, 100].
    limit: Option<i64>,
}

async fn search(
    State(s): State<Arc<VaultState>>,
    Query(q): Query<SearchQ>,
) -> Result<Json<VaultSearchResult>, VaultError> {
    let Some(vs) = &s.search else {
        return Err(VaultError::Unavailable("vault index is not available".into()));
    };
    {
        let _guard = vs.update_lock.lock().await;
        index::update(&vs.pool, &s.root, &vs.exclusions)
            .await
            .map_err(|e| VaultError::Io(format!("updating the vault index: {e:#}")))?;
    }
    let bucket = q.bucket.as_deref().filter(|b| !b.is_empty());
    let opts = index::SearchOpts { bucket, limit: q.limit.unwrap_or(20).clamp(1, 100) };
    let result = index::search(&vs.pool, &q.q, &opts, &vs.exclusions)
        .await
        .map_err(|e| VaultError::Io(format!("{e:#}")))?;
    Ok(Json(result))
}

// ─── vault check (ADR-035) ──────────────────────────────────────────────────

async fn open_check_db(s: &VaultState) -> Result<Option<SqlitePool>, VaultError> {
    if !s.check_db.exists() {
        return Ok(None);
    }
    nucleus_core::db::open_read_only(&s.check_db)
        .await
        .map(Some)
        .map_err(|e| VaultError::Io(format!("opening vault_check.db: {e:#}")))
}

/// The latest report with its findings; `null` before the first run.
async fn check_latest(
    State(s): State<Arc<VaultState>>,
) -> Result<Json<Option<CheckReport>>, VaultError> {
    let Some(pool) = open_check_db(&s).await? else { return Ok(Json(None)) };
    let r = check::latest(&pool).await.map_err(|e| VaultError::Io(format!("{e:#}")))?;
    Ok(Json(r))
}

#[derive(Deserialize)]
struct RunsQ {
    /// Defaults to 26 (half a year of weekly runs), clamped to [1, 200].
    limit: Option<i64>,
}

/// Run counts, newest first — the trend.
async fn check_runs(
    State(s): State<Arc<VaultState>>,
    Query(q): Query<RunsQ>,
) -> Result<Json<Vec<CheckRunSummary>>, VaultError> {
    let Some(pool) = open_check_db(&s).await? else { return Ok(Json(vec![])) };
    let runs = check::runs(&pool, q.limit.unwrap_or(26).clamp(1, 200))
        .await
        .map_err(|e| VaultError::Io(format!("{e:#}")))?;
    Ok(Json(runs))
}

// ─── buckets ────────────────────────────────────────────────────────────────

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct Bucket {
    /// Display name (e.g. `0-Inbox`).
    name: String,
    file_count: usize,
}

async fn list_buckets(State(s): State<Arc<VaultState>>) -> Result<Json<Vec<Bucket>>, VaultError> {
    let mut out = Vec::new();
    let mut entries = match tokio::fs::read_dir(&s.root).await {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Json(out)),
        Err(e) => return Err(VaultError::Io(e.to_string())),
    };
    while let Some(dirent) = entries
        .next_entry()
        .await
        .map_err(|e| VaultError::Io(e.to_string()))?
    {
        let path = dirent.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) if !skip_top_level(n) => n.to_string(),
            _ => continue,
        };
        let ft = match dirent.file_type().await {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if !ft.is_dir() {
            continue;
        }
        let file_count = count_md_recursive(&path).await;
        out.push(Bucket { name, file_count });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(out))
}

async fn count_md_recursive(dir: &Path) -> usize {
    let mut count = 0;
    let mut stack: Vec<PathBuf> = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&d).await {
            Ok(e) => e,
            Err(_) => continue,
        };
        while let Ok(Some(dirent)) = entries.next_entry().await {
            let path = dirent.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            if skip_file_or_dir(name) {
                continue;
            }
            match dirent.file_type().await {
                Ok(ft) if ft.is_dir() => stack.push(path),
                Ok(ft) if ft.is_file() && name.ends_with(".md") => count += 1,
                _ => {}
            }
        }
    }
    count
}

// ─── recent ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct RecentQ {
    /// Restrict to one bucket (top-level folder name, e.g. `0-Inbox`).
    bucket: Option<String>,
    /// How many entries to return. Defaults to 30, clamped to [1, 200].
    limit: Option<usize>,
}

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct VaultFile {
    /// Path relative to vault root (e.g. `3-Projects/Foo/index.md`).
    relpath: String,
    /// Top-level bucket name (e.g. `3-Projects`). Empty for root-level files.
    bucket: String,
    /// File mtime in unix epoch seconds.
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    mtime_unix: i64,
    #[ts(type = "number")]
    bytes: u64,
    /// Absolute path. Used to fetch the file body separately.
    path: String,
}

async fn list_recent(
    State(s): State<Arc<VaultState>>,
    Query(q): Query<RecentQ>,
) -> Result<Json<Vec<VaultFile>>, VaultError> {
    let limit = q.limit.unwrap_or(30).clamp(1, 200);
    let scan_root = match &q.bucket {
        Some(b) => s.root.join(b),
        None => s.root.clone(),
    };

    let mut files: Vec<VaultFile> = Vec::new();
    let mut stack: Vec<PathBuf> = vec![scan_root];
    while let Some(d) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&d).await {
            Ok(e) => e,
            Err(_) => continue,
        };
        while let Ok(Some(dirent)) = entries.next_entry().await {
            let path = dirent.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            if skip_file_or_dir(name) {
                continue;
            }
            let ft = match dirent.file_type().await {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if !name.ends_with(".md") {
                continue;
            }
            let meta = match dirent.metadata().await {
                Ok(m) => m,
                Err(_) => continue,
            };
            let mtime_unix = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let relpath = path
                .strip_prefix(&s.root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| path.to_string_lossy().into_owned());
            let bucket = relpath
                .split('/')
                .next()
                .filter(|s| !s.is_empty() && s.contains('-'))
                .unwrap_or("")
                .to_string();
            files.push(VaultFile {
                relpath,
                bucket,
                mtime_unix,
                bytes: meta.len(),
                path: path.to_string_lossy().into_owned(),
            });
        }
    }

    files.sort_by(|a, b| b.mtime_unix.cmp(&a.mtime_unix));
    files.truncate(limit);
    Ok(Json(files))
}

// ─── file body ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct FileQ {
    path: String,
}

async fn get_file(
    State(s): State<Arc<VaultState>>,
    Query(q): Query<FileQ>,
) -> Result<String, VaultError> {
    // Absolute (the recent feed) or vault-relative (search results).
    let requested = PathBuf::from(&q.path);
    let requested = if requested.is_relative() { s.root.join(requested) } else { requested };
    let canonical = tokio::fs::canonicalize(&requested)
        .await
        .map_err(|e| VaultError::Io(format!("canonicalizing {}: {}", q.path, e)))?;
    let canon_root = tokio::fs::canonicalize(&s.root)
        .await
        .map_err(|e| VaultError::Io(format!("canonicalizing root: {}", e)))?;
    if !canonical.starts_with(&canon_root) {
        return Err(VaultError::OutsideRoot);
    }
    if canonical.extension().and_then(|e| e.to_str()) != Some("md") {
        return Err(VaultError::OutsideRoot);
    }
    tokio::fs::read_to_string(&canonical)
        .await
        .map_err(|e| VaultError::Io(format!("reading {}: {}", canonical.display(), e)))
}

// ─── filters ────────────────────────────────────────────────────────────────

/// Skip top-level dirs that aren't user content: dot-dirs (`.obsidian`) and
/// `node_modules`. Files are dropped by the is_dir gate in `list_buckets`.
fn skip_top_level(name: &str) -> bool {
    name.starts_with('.') || name == "node_modules"
}

/// Skip anything inside the vault we don't want to surface:
/// dotfiles (including .obsidian/), pending-state files, and the
/// home-dashboard markdown we built in ADR-014.
fn skip_file_or_dir(name: &str) -> bool {
    if name.starts_with('.') {
        return true;
    }
    if name.starts_with('_') {
        return true; // _pending.md, _original-capture.md, etc.
    }
    if name == "Home.md" || name == "Home-projects.base" || name == "Home-areas.base" {
        return true; // dashboard scaffolding, not vault content
    }
    false
}

// ─── errors ─────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum VaultError {
    Io(String),
    OutsideRoot,
    Unavailable(String),
}

impl IntoResponse for VaultError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::Io(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
            Self::Unavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, m),
            Self::OutsideRoot => (
                StatusCode::FORBIDDEN,
                "path is not inside the vault root".to_string(),
            ),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn get_json(app: &Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let res = app.clone().oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null))
    }

    /// Synthetic vault; names and bodies are invented.
    #[tokio::test]
    async fn search_and_check_routes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("vault");
        for (rel, text) in [
            ("3-Projects/Alpha/index.md", "# Alpha\n\nrocket engine hub\n"),
            ("4-Areas/Homelab/router.md", "rocket router login\n"),
            ("6-Slipbox/keys.md", "rocket\napi_key: abc\n"),
        ] {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        let exclusions = Exclusions::new(&[], "").unwrap();
        let pool = index::open_at(&tmp.path().join("idx.db")).await.unwrap();
        let check_db = tmp.path().join("check.db");
        let state = Arc::new(VaultState {
            root: root.clone(),
            search: Some(VaultSearch { pool, exclusions: exclusions.clone(), update_lock: Default::default() }),
            check_db: check_db.clone(),
        });
        let app = router(state);

        let (status, body) = get_json(&app, "/search?q=rocket").await;
        assert_eq!(status, StatusCode::OK);
        let hits = body["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "credential notes must not be returned: {body}");
        assert_eq!(hits[0]["display"], "Alpha/index.md");
        assert_eq!(hits[0]["bucket"], "3-Projects");

        // A hit's relative path opens through /file; traversal is refused.
        let (status, _) = get_json(&app, "/file?path=3-Projects/Alpha/index.md").await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = get_json(&app, "/file?path=../idx.db").await;
        assert_ne!(status, StatusCode::OK);

        // No run yet → null / empty.
        let (_, latest) = get_json(&app, "/check/latest").await;
        assert!(latest.is_null());
        let (_, runs) = get_json(&app, "/check/runs").await;
        assert_eq!(runs, serde_json::json!([]));

        // After a recorded run the report comes back.
        let opts = check::CheckOptions::from_config(&Default::default(), false, "manual").unwrap();
        let report = check::run(&root, &exclusions, &opts).unwrap();
        let wpool = check::open_at(&check_db).await.unwrap();
        check::record(&wpool, &report).await.unwrap();
        let (_, latest) = get_json(&app, "/check/latest").await;
        assert_eq!(latest["notes_scanned"], 1);
        assert_eq!(latest["trigger"], "manual");
        let (_, runs) = get_json(&app, "/check/runs?limit=5").await;
        assert_eq!(runs.as_array().unwrap().len(), 1);
    }
}
