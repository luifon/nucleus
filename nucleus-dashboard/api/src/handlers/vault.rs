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
//! search it brings the index up to date by running the index writer,
//! `nucleus vault-search --reindex`, as a subprocess, then reads the index
//! read-only. [`IndexRefresh`] limits that to one subprocess at a time,
//! shared by every search that arrives while it runs, and skips it while
//! the index is known to be current.

use axum::{
    extract::{Query, State},
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Json, Response},
    routing::get,
    Router,
};
use futures::future::{BoxFuture, FutureExt, Shared};
use nucleus_core::vault::access::{self, AccessError};
use nucleus_core::vault::check::{self, CheckReport, CheckRunSummary};
use nucleus_core::vault::exclude::Exclusions;
use nucleus_core::vault::index::{self, VaultSearchResult};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    /// `memory/vault_index.db`, opened read-only per request.
    pub index_db: PathBuf,
    pub refresh: IndexRefresh,
}

/// A vault watermark ([`nucleus_core::vault::scan::watermark`]).
type Watermark = u64;

/// Result of one index update, shared by every search waiting on it. The
/// error is logged once, by the update task.
type UpdateResult = Result<(), ()>;

/// One running index update.
#[derive(Clone)]
struct Flight {
    /// The vault watermark taken just before the update started.
    watermark: Watermark,
    done: Shared<BoxFuture<'static, UpdateResult>>,
}

#[derive(Default)]
struct RefreshState {
    in_flight: Option<Flight>,
    /// Start time and watermark of the last update that succeeded.
    last_ok: Option<(Instant, Watermark)>,
}

/// Why a search could not get a current index.
#[derive(Debug)]
pub enum RefreshError {
    /// An update is running and did not finish within the wait bound.
    Busy,
    /// The update failed (logged by the update task).
    Failed,
}

/// Keeps the index current for searches with at most one update running.
///
/// - **Single flight.** An update runs as its own task. A search that
///   arrives while an update runs for the same vault watermark waits on
///   that update's result instead of starting another. A search whose
///   watermark differs (a note changed after the update started) waits for
///   it to finish and then starts the next one.
/// - **Bounded wait.** A search waits at most `max_wait` in total, then
///   gets [`RefreshError::Busy`]. The update keeps running; it is never
///   cancelled by a client that gave up, and later searches share it.
/// - **Freshness.** No update runs when the last successful one started
///   less than `fresh_for` ago and the watermark has not changed since.
pub struct IndexRefresh {
    reindex: Reindex,
    fresh_for: Duration,
    max_wait: Duration,
    state: Arc<Mutex<RefreshState>>,
}

impl IndexRefresh {
    pub fn new(reindex: Reindex, fresh_for: Duration, max_wait: Duration) -> Self {
        Self { reindex, fresh_for, max_wait, state: Default::default() }
    }

    /// Return once the index reflects the vault at `watermark`.
    pub async fn ensure_current(&self, watermark: Watermark) -> Result<(), RefreshError> {
        let deadline = tokio::time::Instant::now() + self.max_wait;
        loop {
            let flight = {
                let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
                if let Some((at, w)) = st.last_ok {
                    if w == watermark && at.elapsed() < self.fresh_for {
                        return Ok(());
                    }
                }
                match &st.in_flight {
                    Some(f) => f.clone(),
                    None => {
                        let f = self.start(watermark);
                        st.in_flight = Some(f.clone());
                        f
                    }
                }
            };
            let result = tokio::time::timeout_at(deadline, flight.done.clone())
                .await
                .map_err(|_| RefreshError::Busy)?;
            if flight.watermark == watermark {
                return result.map_err(|()| RefreshError::Failed);
            }
            // That update started before the vault reached this state; check
            // again, which starts the next update if none is running.
        }
    }

    fn start(&self, watermark: Watermark) -> Flight {
        let reindex = self.reindex.clone();
        let state = self.state.clone();
        let started = Instant::now();
        let task = tokio::spawn(async move {
            let result: UpdateResult = reindex().await.map_err(|e| {
                tracing::warn!("vault: index update failed: {e:#}");
            });
            let mut st = state.lock().unwrap_or_else(|p| p.into_inner());
            st.in_flight = None;
            if result.is_ok() {
                st.last_ok = Some((started, watermark));
            }
            result
        });
        let done = async move {
            task.await.unwrap_or_else(|e| {
                tracing::warn!("vault: index update task failed: {e}");
                Err(())
            })
        }
        .boxed()
        .shared();
        Flight { watermark, done }
    }
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
    let (root, ex) = (s.root.clone(), s.rules()?);
    let watermark = blocking(move || nucleus_core::vault::scan::watermark(&root, &ex))
        .await?
        .map_err(|e| {
            tracing::warn!("vault: reading the vault for the index watermark: {e:#}");
            VaultError::Io("reading the vault failed".into())
        })?;
    vs.refresh.ensure_current(watermark).await.map_err(|e| match e {
        RefreshError::Busy => VaultError::Unavailable("the vault index is being updated; try again shortly".into()),
        RefreshError::Failed => VaultError::Unavailable("updating the vault index failed".into()),
    })?;
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

/// The latest report with its findings; `null` before the first run. The
/// stored report is filtered through the exclusion rules as they are at
/// this request ([`check::visible_report`]): a note excluded after the run
/// (by a new glob or by a credential written into it) is not named.
async fn check_latest(State(s): State<Arc<VaultState>>) -> Result<Response, VaultError> {
    let ex = s.rules()?;
    let Some(pool) = open_check_db(&s).await? else { return Ok(no_store(Json(None::<CheckReport>).into_response())) };
    let r = check::latest(&pool).await.map_err(|e| VaultError::Io(format!("{e:#}")))?;
    pool.close().await;
    let root = s.root.clone();
    let r = blocking(move || {
        r.map(|r| {
            let mut seen: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
            check::visible_report(r, |p| {
                *seen.entry(p.to_string()).or_insert_with(|| access::may_show(&root, &ex, p))
            })
        })
    })
    .await?;
    Ok(no_store(Json(r).into_response()))
}

#[derive(Deserialize)]
struct RunsQ {
    /// Defaults to 26 (half a year of weekly runs), clamped to [1, 200].
    limit: Option<i64>,
}

/// Run counts, newest first — the trend. Counts only, no paths: the counts
/// are the ones each run recorded, under the rules of that run.
async fn check_runs(State(s): State<Arc<VaultState>>, Query(q): Query<RunsQ>) -> Result<Response, VaultError> {
    let Some(pool) = open_check_db(&s).await? else {
        return Ok(no_store(Json(Vec::<CheckRunSummary>::new()).into_response()));
    };
    let runs = check::runs(&pool, q.limit.unwrap_or(26).clamp(1, 200))
        .await
        .map_err(|e| VaultError::Io(format!("{e:#}")))?;
    pool.close().await;
    Ok(no_store(Json(runs).into_response()))
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
    use std::time::Duration;
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
        let (app, root, ws, _) = app_with(tmp, Duration::ZERO, Duration::from_secs(60), Duration::from_secs(30));
        (app, root, ws)
    }

    /// [`app`] with the refresh settings and an update delay; also returns
    /// the number of updates that started.
    fn app_with(
        tmp: &Path,
        delay: Duration,
        fresh_for: Duration,
        max_wait: Duration,
    ) -> (Router, PathBuf, PathBuf, Arc<std::sync::atomic::AtomicUsize>) {
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
        let started = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = started.clone();
        let reindex: Reindex = Arc::new(move || {
            let (ws, db, lock, vault) = (ws2.clone(), db2.clone(), lock.clone(), vault2.clone());
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                tokio::time::sleep(delay).await;
                let w = index::Writer::open_at(&ws, &db, &lock).await?;
                w.update(&vault).await?;
                w.pool().close().await;
                Ok(())
            })
        });
        let state = Arc::new(VaultState {
            root: root.clone(),
            workspace_root: ws.clone(),
            search: Some(VaultSearch { index_db, refresh: IndexRefresh::new(reindex, fresh_for, max_wait) }),
            check_db: tmp.join("check.db"),
        });
        (router(state), root, ws, started)
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

    /// Round 3, item 2: the stored report is filtered through the rules at
    /// request time. A note excluded after the run (by a glob, or by a
    /// credential written into it) disappears from `/check/latest` at the
    /// next request, with its count; both check routes are `no-store`.
    #[tokio::test]
    async fn check_latest_applies_current_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, root, ws) = app(tmp.path());
        let ex = Exclusions::new(&[], "").unwrap();
        let opts = check::CheckOptions::from_config(&Default::default(), false, "manual").unwrap();
        let wpool = check::open_at(&tmp.path().join("check.db")).await.unwrap();
        check::record(&wpool, &check::run(&root, &ex, &opts).unwrap()).await.unwrap();

        let (status, headers, body) = get_raw(&app, "/check/latest").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
        assert!(body.contains("6-Slipbox/idea.md") && body.contains("3-Projects/Alpha/index.md"), "{body}");
        let orphans = |b: &str| serde_json::from_str::<serde_json::Value>(b).unwrap()["counts"]["orphans"].as_i64().unwrap();
        let before = orphans(&body);

        write(&ws, "nucleus.toml", "[vault_search]\nexclude = [\"6-Slipbox/**\"]\n");
        let (_, _, body) = get_raw(&app, "/check/latest").await;
        assert!(!body.contains("6-Slipbox/idea.md"), "{body}");
        assert!(body.contains("3-Projects/Alpha/index.md"), "{body}");
        assert_eq!(orphans(&body), before - 1);

        write(&root, "3-Projects/Alpha/index.md", "# Alpha\n\npassword: correct horse battery staple\n");
        let (_, _, body) = get_raw(&app, "/check/latest").await;
        assert!(!body.contains("3-Projects/Alpha/index.md"), "{body}");

        let (_, headers, _) = get_raw(&app, "/check/runs").await;
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
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

    fn updates(n: &std::sync::atomic::AtomicUsize) -> usize {
        n.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A burst of concurrent searches runs one index update and every
    /// search gets its result; a second burst on an unchanged vault runs
    /// none; a changed note runs exactly one more.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn search_burst_shares_one_update() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, root, _, started) =
            app_with(tmp.path(), Duration::from_millis(300), Duration::from_secs(60), Duration::from_secs(30));

        let burst = |app: Router| async move {
            let reqs = (0..25).map(|_| {
                let app = app.clone();
                tokio::spawn(async move { get_json(&app, "/search?q=rocket").await })
            });
            futures::future::join_all(reqs).await.into_iter().map(|r| r.unwrap()).collect::<Vec<_>>()
        };
        for (status, body) in burst(app.clone()).await {
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["hits"].as_array().unwrap().len(), 3, "{body}");
        }
        assert_eq!(updates(&started), 1, "one update for the whole burst");

        for (status, _) in burst(app.clone()).await {
            assert_eq!(status, StatusCode::OK);
        }
        assert_eq!(updates(&started), 1, "fresh and unchanged: no update");

        write(&root, "6-Slipbox/new.md", "# New\n\nrocket new\n");
        let (status, body) = get_json(&app, "/search?q=rocket").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["hits"].as_array().unwrap().len(), 4, "{body}");
        assert_eq!(updates(&started), 2, "a changed note: one more update");
    }

    /// With the freshness window elapsed, an unchanged vault is updated
    /// again (the watermark alone does not skip forever).
    #[tokio::test]
    async fn stale_window_updates_again() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _, _, started) = app_with(tmp.path(), Duration::ZERO, Duration::ZERO, Duration::from_secs(30));
        for _ in 0..3 {
            assert_eq!(get_json(&app, "/search?q=rocket").await.0, StatusCode::OK);
        }
        assert_eq!(updates(&started), 3);
    }

    /// A search waits at most the bound for a running update and answers
    /// 503; the update keeps running (it is not started again by the
    /// waiting searches) and the next search uses its result.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn search_wait_is_bounded() {
        let tmp = tempfile::tempdir().unwrap();
        let (app, _, _, started) =
            app_with(tmp.path(), Duration::from_millis(800), Duration::from_secs(60), Duration::from_millis(100));
        let t0 = std::time::Instant::now();
        let reqs = (0..10).map(|_| {
            let app = app.clone();
            tokio::spawn(async move { get_json(&app, "/search?q=rocket").await })
        });
        for r in futures::future::join_all(reqs).await {
            let (status, body) = r.unwrap();
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
            assert!(body["error"].as_str().unwrap().contains("being updated"), "{body}");
        }
        assert!(t0.elapsed() < Duration::from_millis(600), "waited {:?}", t0.elapsed());
        assert_eq!(updates(&started), 1);

        tokio::time::sleep(Duration::from_millis(1000)).await;
        let (status, body) = get_json(&app, "/search?q=rocket").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(updates(&started), 1, "the finished update is reused");
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
