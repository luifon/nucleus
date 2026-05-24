//! Vault surface — chronological feed of writes into the Obsidian
//! vault (per ADR-005). Backed by filesystem mtime — no audit log
//! exists for brain-dump applies, so the feed reflects "what files
//! changed recently" rather than "what the apply pipeline did".
//! Good enough for the operator's "what did the bot write?" question;
//! see ADR-015 §"Future work" for the audit-log alternative.

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
use std::time::SystemTime;

#[derive(Clone)]
pub struct VaultState {
    pub root: PathBuf,
}

pub fn router(state: Arc<VaultState>) -> Router {
    Router::new()
        .route("/recent", get(list_recent))
        .route("/file", get(get_file))
        .route("/buckets", get(list_buckets))
        .with_state(state)
}

// ─── buckets ────────────────────────────────────────────────────────────────

#[derive(Serialize)]
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

#[derive(Serialize)]
struct VaultFile {
    /// Path relative to vault root (e.g. `3-Projects/Foo/index.md`).
    relpath: String,
    /// Top-level bucket name (e.g. `3-Projects`). Empty for root-level files.
    bucket: String,
    /// File mtime in unix epoch seconds.
    mtime_unix: i64,
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
    let requested = PathBuf::from(&q.path);
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

/// Skip top-level entries that aren't user content: Obsidian's
/// settings dir, dot-files, and the vault-root home/workspace files.
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
}

impl IntoResponse for VaultError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::Io(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
            Self::OutsideRoot => (
                StatusCode::FORBIDDEN,
                "path is not inside the vault root".to_string(),
            ),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}
