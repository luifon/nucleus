//! Vault surface — chronological feed of writes into the Obsidian
//! vault (per ADR-005). Backed by filesystem mtime — no audit log
//! exists for brain-dump applies, so the feed reflects "what files
//! changed recently" rather than "what the apply pipeline did".
//! Good enough for the operator's "what did the bot write?" question;
//! see ADR-015 §"Scope" for the audit-log alternative.
//!
//! ADR-035 adds full-text search (`/search`) and the weekly vault check
//! report (`/check/latest`, `/check/runs`, read-only from
//! `memory/vault_check.db`, which only `vault-check` writes).
//!
//! Every route applies the vault exclusion rules as nucleus.toml states
//! them at the time of the request ([`Exclusions::load`]): an excluded or
//! credential note is never opened, listed, counted by name, or returned
//! as a search hit. `/file` and `/recent` take and return vault-relative
//! paths only.
//!
//! This process never writes `memory/vault_index.db` (ADR-020): before a
//! search it runs the index writer, `nucleus vault-search --reindex`, as a
//! subprocess, then reads the index read-only.

use axum::{
    extract::{Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::get,
    Router,
};
use futures::future::BoxFuture;
use nucleus_core::vault::access::{self, AccessError};
use nucleus_core::vault::check::{self, CheckReport, CheckRunSummary};
use nucleus_core::vault::exclude::Exclusions;
use nucleus_core::vault::index::{self, VaultSearchResult};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::Arc;

pub struct VaultState {
    pub root: PathBuf,
    /// Where nucleus.toml lives; the exclusion rules are read from it on
    /// every request.
    pub workspace_root: PathBuf,
    /// `None` disables `/search` (503).
    pub search: Option<VaultSearch>,
    /// `memory/vault_check.db`, opened read-only per request.
    pub check_db: PathBuf,
}

/// Brings the index up to date by running its writer.
pub type Reindex = Arc<dyn Fn() -> BoxFuture<'static, anyhow::Result<()>> + Send + Sync>;

pub struct VaultSearch {
    pub reindex: Reindex,
    /// `memory/vault_index.db`, opened read-only per request.
    pub index_db: PathBuf,
    /// One reindex subprocess at a time from this process.
    pub reindex_lock: tokio::sync::Mutex<()>,
}

/// The production [`Reindex`]: `<this binary> vault-search --reindex`,
/// run in the workspace root. Its output stays in this process's log; a
/// client sees only that the update failed.
pub fn subprocess_reindex(workspace_root: PathBuf) -> anyhow::Result<Reindex> {
    let exe = std::env::current_exe()?;
    Ok(Arc::new(move || {
        let exe = exe.clone();
        let ws = workspace_root.clone();
        Box::pin(async move {
            let run = tokio::process::Command::new(&exe)
                .args(["vault-search", "--reindex"])
                .current_dir(&ws)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .output();
            let out = tokio::time::timeout(std::time::Duration::from_secs(120), run)
                .await
                .map_err(|_| anyhow::anyhow!("vault-search --reindex timed out"))??;
            if !out.status.success() {
                anyhow::bail!(
                    "vault-search --reindex exited with {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok(())
        })
    }))
}

impl VaultState {
    /// The exclusion rules as they are now. Failing to load them fails the
    /// request (closed), never falls back to weaker rules.
    fn rules(&self) -> Result<Exclusions, VaultError> {
        Exclusions::load(&self.workspace_root).map_err(|e| {
            tracing::warn!("vault: exclusion rules not loadable: {e:#}");
            VaultError::Unavailable("vault exclusion rules are not loadable".into())
        })
    }
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

/// Responses carrying note content or paths are not cached.
fn no_store(mut r: Response) -> Response {
    r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    r
}

/// Run filesystem work off the async runtime.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, VaultError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| VaultError::Io(format!("vault task: {e}")))
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

async fn search(State(s): State<Arc<VaultState>>, Query(q): Query<SearchQ>) -> Result<Response, VaultError> {
    let Some(vs) = &s.search else {
        return Err(VaultError::Unavailable("vault index is not available".into()));
    };
    {
        let _guard = vs.reindex_lock.lock().await;
        (vs.reindex)().await.map_err(|e| {
            tracing::warn!("vault: index update failed: {e:#}");
            VaultError::Unavailable("updating the vault index failed".into())
        })?;
    }
    // Rules loaded after the update: at least as new as the ones it used.
    let ex = s.rules()?;
    let Some(pool) = index::open_read_only_at(&vs.index_db)
        .await
        .map_err(|e| VaultError::Io(format!("opening the vault index: {e:#}")))?
    else {
        return Ok(no_store(Json(VaultSearchResult { hits: vec![], mode: "all".into() }).into_response()));
    };
    let bucket = q.bucket.as_deref().filter(|b| !b.is_empty());
    let opts = index::SearchOpts { bucket, limit: q.limit.unwrap_or(20).clamp(1, 100) };
    let mut result = index::search(&pool, &q.q, &opts, &ex)
        .await
        .map_err(|e| VaultError::Io(format!("{e:#}")))?;
    pool.close().await;
    // Every hit must still be openable under the current rules: the same
    // check `/file` makes, so a hit never names a note `/file` refuses.
    let root = s.root.clone();
    let hits = std::mem::take(&mut result.hits);
    result.hits = blocking(move || {
        hits.into_iter().filter(|h| access::open_note(&root, &ex, &h.path).is_ok()).collect()
    })
    .await?;
    Ok(no_store(Json(result).into_response()))
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
    let ex = s.rules()?;
    let root = s.root.clone();
    if !root.exists() {
        return Ok(Json(vec![]));
    }
    let buckets = blocking(move || access::buckets(&root, &ex))
        .await?
        .map_err(|e| VaultError::Io(format!("{e:#}")))?;
    Ok(Json(buckets.into_iter().map(|(name, file_count)| Bucket { name, file_count }).collect()))
}

// ─── recent ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
struct RecentQ {
    /// Restrict to one bucket or folder (vault-relative, e.g. `0-Inbox`).
    bucket: Option<String>,
    /// How many entries to return. Defaults to 30, clamped to [1, 200].
    limit: Option<usize>,
}

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct VaultFile {
    /// Path relative to vault root (e.g. `3-Projects/Foo/index.md`). Pass it
    /// to `/file` to fetch the body.
    relpath: String,
    /// Top-level bucket name (e.g. `3-Projects`). Empty for root-level files.
    bucket: String,
    /// File mtime in unix epoch seconds.
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    mtime_unix: i64,
    #[ts(type = "number")]
    bytes: u64,
    /// Name of the vault folder, for `obsidian://open?vault=` links.
    vault_name: String,
}

/// Paths the recent feed leaves out although they may be opened: pipeline
/// scratch files (`_pending.md`, `_original-capture.md`) and the home
/// dashboard notes (ADR-014).
pub fn hidden_from_feed(rel: &str) -> bool {
    let file = rel.rsplit('/').next().unwrap_or(rel);
    rel.split('/').any(|c| c.starts_with('_'))
        || matches!(file, "Home.md" | "Home-projects.base" | "Home-areas.base")
}

/// Top-level bucket of a relative path, when it is a PARA bucket (`3-X`).
pub fn bucket_label(rel: &str) -> String {
    rel.split_once('/')
        .map(|(top, _)| top)
        .filter(|s| s.contains('-'))
        .unwrap_or("")
        .to_string()
}

async fn list_recent(State(s): State<Arc<VaultState>>, Query(q): Query<RecentQ>) -> Result<Response, VaultError> {
    let limit = q.limit.unwrap_or(30).clamp(1, 200);
    let ex = s.rules()?;
    let root = s.root.clone();
    let vault_name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let bucket = q.bucket.filter(|b| !b.trim().is_empty());
    let notes = blocking(move || -> Result<Vec<access::RecentNote>, VaultError> {
        let folder = match bucket {
            None => None,
            Some(b) => match access::resolve_folder(&root, &ex, &b) {
                Ok(f) => Some(f),
                // An excluded folder looks like a missing one.
                Err(AccessError::Excluded | AccessError::NotFound) => return Ok(vec![]),
                Err(e) => return Err(e.into()),
            },
        };
        access::recent(&root, &ex, folder.as_deref(), limit, hidden_from_feed)
            .map_err(|e| VaultError::Io(format!("{e:#}")))
    })
    .await??;
    let files: Vec<VaultFile> = notes
        .into_iter()
        .map(|n| VaultFile {
            bucket: bucket_label(&n.rel),
            relpath: n.rel,
            mtime_unix: n.mtime,
            bytes: n.size,
            vault_name: vault_name.clone(),
        })
        .collect();
    Ok(no_store(Json(files).into_response()))
}

// ─── file body ──────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct FileQ {
    /// Vault-relative path.
    path: String,
}

async fn get_file(State(s): State<Arc<VaultState>>, Query(q): Query<FileQ>) -> Result<Response, VaultError> {
    let ex = s.rules()?;
    let root = s.root.clone();
    let note = blocking(move || access::open_note(&root, &ex, &q.path)).await??;
    let mut r = note.text.into_response();
    r.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    Ok(no_store(r))
}

// ─── errors ─────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum VaultError {
    Io(String),
    Access(AccessError),
    Unavailable(String),
}

impl From<AccessError> for VaultError {
    fn from(e: AccessError) -> Self {
        Self::Access(e)
    }
}

impl IntoResponse for VaultError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::Io(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
            Self::Unavailable(m) => (StatusCode::SERVICE_UNAVAILABLE, m),
            Self::Access(e) => {
                let code = match &e {
                    AccessError::Invalid => StatusCode::BAD_REQUEST,
                    AccessError::NotMarkdown => StatusCode::FORBIDDEN,
                    // Same answer for excluded and missing notes.
                    AccessError::Excluded | AccessError::NotFound => StatusCode::NOT_FOUND,
                    AccessError::TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
                    AccessError::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
                };
                // I/O error text can carry absolute paths; keep it in the log.
                if let AccessError::Io(err) = &e {
                    tracing::warn!("vault: {err}");
                    return no_store((code, Json(serde_json::json!({ "error": "vault read failed" }))).into_response());
                }
                (code, e.to_string())
            }
        };
        no_store((code, Json(serde_json::json!({ "error": msg }))).into_response())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use std::path::Path;
    use tower::ServiceExt;

    async fn get_raw(app: &Router, uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
        let res = app.clone().oneshot(Request::get(uri).body(Body::empty()).unwrap()).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX).await.unwrap();
        (status, headers, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn get_json(app: &Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let (status, _, body) = get_raw(app, uri).await;
        (status, serde_json::from_str(&body).unwrap_or(serde_json::Value::Null))
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    /// Synthetic vault and workspace; names and bodies are invented. The
    /// reindex runs core's writer in-process (production runs it as a
    /// subprocess).
    fn app(tmp: &Path) -> (Router, PathBuf, PathBuf) {
        let root = tmp.join("vault");
        let ws = tmp.join("ws");
        for (rel, text) in [
            ("3-Projects/Alpha/index.md", "# Alpha\n\nrocket engine hub\n"),
            ("4-Areas/Homelab/router.md", "rocket router login\n"),
            ("6-Slipbox/keys.md", "rocket\napi_key: abc\n"),
            ("6-Slipbox/idea.md", "# Idea\n\nrocket idea\n"),
            ("0-Inbox/_pending.md", "rocket pending\n"),
        ] {
            write(&root, rel, text);
        }
        write(&ws, "nucleus.toml", "[vault_search]\nexclude = []\n");
        let index_db = tmp.join("idx.db");
        let (ws2, db2, lock, vault2) = (ws.clone(), index_db.clone(), tmp.join("idx.lock"), root.clone());
        let reindex: Reindex = Arc::new(move || {
            let (ws, db, lock, vault) = (ws2.clone(), db2.clone(), lock.clone(), vault2.clone());
            Box::pin(async move {
                let w = index::Writer::open_at(&ws, &db, &lock).await?;
                w.update(&vault).await?;
                w.pool().close().await;
                Ok(())
            })
        });
        let state = Arc::new(VaultState {
            root: root.clone(),
            workspace_root: ws.clone(),
            search: Some(VaultSearch { reindex, index_db, reindex_lock: Default::default() }),
            check_db: tmp.join("check.db"),
        });
        (router(state), root, ws)
    }

    #[tokio::test]
    async fn search_and_check_routes() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, root, _) = app(tmp.path());

        let (status, body) = get_json(&app, "/search?q=rocket").await;
        assert_eq!(status, StatusCode::OK);
        let mut paths: Vec<&str> = body["hits"].as_array().unwrap().iter().map(|h| h["path"].as_str().unwrap()).collect();
        paths.sort();
        assert_eq!(paths, vec!["0-Inbox/_pending.md", "3-Projects/Alpha/index.md", "6-Slipbox/idea.md"], "{body}");

        // No run yet → null / empty.
        let (_, latest) = get_json(&app, "/check/latest").await;
        assert!(latest.is_null());
        let (_, runs) = get_json(&app, "/check/runs").await;
        assert_eq!(runs, serde_json::json!([]));

        // After a recorded run the report comes back.
        let ex = Exclusions::new(&[], "").unwrap();
        let opts = check::CheckOptions::from_config(&Default::default(), false, "manual").unwrap();
        let report = check::run(&root, &ex, &opts).unwrap();
        let wpool = check::open_at(&tmp.path().join("check.db")).await.unwrap();
        check::record(&wpool, &report).await.unwrap();
        let (_, latest) = get_json(&app, "/check/latest").await;
        assert_eq!(latest["notes_scanned"], 3);
        assert_eq!(latest["trigger"], "manual");
        let (_, runs) = get_json(&app, "/check/runs?limit=5").await;
        assert_eq!(runs.as_array().unwrap().len(), 1);
    }

    /// H1: excluded notes cannot be opened or listed; paths are relative.
    #[tokio::test]
    async fn file_and_recent_apply_the_exclusions() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, root, _) = app(tmp.path());

        let (status, headers, body) = get_raw(&app, "/file?path=3-Projects/Alpha/index.md").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("rocket engine hub"));
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");

        for (uri, code) in [
            ("/file?path=4-Areas/Homelab/router.md", StatusCode::NOT_FOUND),
            ("/file?path=6-Slipbox/keys.md", StatusCode::NOT_FOUND),
            ("/file?path=6-Slipbox/missing.md", StatusCode::NOT_FOUND),
            ("/file?path=../idx.db", StatusCode::BAD_REQUEST),
            ("/file?path=3-Projects/../4-Areas/Homelab/router.md", StatusCode::BAD_REQUEST),
        ] {
            let (status, headers, body) = get_raw(&app, uri).await;
            assert_eq!(status, code, "{uri}: {body}");
            assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
            assert!(!body.contains("login") && !body.contains("abc"), "{uri}: {body}");
        }
        let abs = format!("/file?path={}", root.join("3-Projects/Alpha/index.md").display());
        assert_eq!(get_raw(&app, &abs).await.0, StatusCode::BAD_REQUEST);

        // A symlink with an innocent name pointing at an excluded note.
        std::os::unix::fs::symlink(root.join("4-Areas/Homelab/router.md"), root.join("6-Slipbox/link.md")).unwrap();
        assert_eq!(get_raw(&app, "/file?path=6-Slipbox/link.md").await.0, StatusCode::NOT_FOUND);

        let (status, headers, body) = get_raw(&app, "/recent").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
        let files: serde_json::Value = serde_json::from_str(&body).unwrap();
        let mut rels: Vec<&str> = files.as_array().unwrap().iter().map(|f| f["relpath"].as_str().unwrap()).collect();
        rels.sort();
        assert_eq!(rels, vec!["3-Projects/Alpha/index.md", "6-Slipbox/idea.md"]);
        assert!(!body.contains(&tmp.path().display().to_string()), "absolute path leaked: {body}");
        assert_eq!(files[0]["vault_name"], "vault");

        let (_, buckets) = get_json(&app, "/buckets").await;
        let names: Vec<&str> = buckets.as_array().unwrap().iter().map(|b| b["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["0-Inbox", "3-Projects", "4-Areas", "6-Slipbox"]);
    }

    /// M: `/recent?bucket=` cannot leave the vault or open an excluded folder.
    #[tokio::test]
    async fn recent_bucket_is_confined() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _, _) = app(tmp.path());
        write(tmp.path(), "outside/notes.md", "# outside\n");
        for b in ["..", "../outside", "/", "/tmp", "3-Projects/../..", "./3-Projects"] {
            let (status, body) = get_json(&app, &format!("/recent?bucket={b}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{b}: {body}");
        }
        let (status, body) = get_json(&app, "/recent?bucket=4-Areas/Homelab").await;
        assert_eq!((status, body), (StatusCode::OK, serde_json::json!([])));
        let (_, body) = get_json(&app, "/recent?bucket=6-Slipbox").await;
        let rels: Vec<&str> = body.as_array().unwrap().iter().map(|f| f["relpath"].as_str().unwrap()).collect();
        assert_eq!(rels, vec!["6-Slipbox/idea.md"]);
    }

    /// H2 (dashboard side): an exclusion added while the dashboard runs
    /// applies at the next request, with no restart.
    #[tokio::test]
    async fn new_exclusions_apply_without_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _, ws) = app(tmp.path());
        assert_eq!(get_raw(&app, "/file?path=6-Slipbox/idea.md").await.0, StatusCode::OK);
        let (_, body) = get_json(&app, "/search?q=idea").await;
        assert_eq!(body["hits"].as_array().unwrap().len(), 1);

        write(&ws, "nucleus.toml", "[vault_search]\nexclude = [\"6-Slipbox/**\"]\n");
        assert_eq!(get_raw(&app, "/file?path=6-Slipbox/idea.md").await.0, StatusCode::NOT_FOUND);
        let (_, body) = get_json(&app, "/search?q=idea").await;
        assert!(body["hits"].as_array().unwrap().is_empty(), "{body}");
        let (_, body) = get_json(&app, "/recent").await;
        assert!(body.as_array().unwrap().iter().all(|f| !f["relpath"].as_str().unwrap().starts_with("6-Slipbox")));

        // An unreadable config fails closed.
        write(&ws, "nucleus.toml", "[vault_search\n");
        assert_eq!(get_raw(&app, "/file?path=3-Projects/Alpha/index.md").await.0, StatusCode::SERVICE_UNAVAILABLE);
    }
}
