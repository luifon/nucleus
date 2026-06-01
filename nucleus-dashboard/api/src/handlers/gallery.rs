//! Image-generation (gallery) surface — ADR-019.
//!
//! Proxies prompts to the local Bonsai FastAPI backend (`POST /generate`
//! → raw PNG bytes), persists each result as a file under `memory/gallery/`
//! plus a row in `memory/gallery.db`, and serves the gallery list. The PNG
//! bytes themselves are served by a `ServeDir` mount at `/gallery/files/*`
//! wired in `main.rs`. The Bonsai backend runs as an always-warm loopback
//! service (`tools/bonsai-serve.sh` via launchd); see ADR-019.

use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

pub struct GalleryState {
    pub pool: SqlitePool,
    /// Directory the generated PNGs are written to (served at /gallery/files).
    pub files_dir: PathBuf,
    /// Base URL of the Bonsai FastAPI backend, e.g. http://127.0.0.1:8093.
    pub bonsai_url: String,
    pub http: reqwest::Client,
}

pub async fn ensure_schema(pool: &SqlitePool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS generated_images (
          id         TEXT PRIMARY KEY,
          prompt     TEXT    NOT NULL,
          seed       INTEGER NOT NULL,
          width      INTEGER NOT NULL,
          height     INTEGER NOT NULL,
          steps      INTEGER NOT NULL,
          created_at TEXT    NOT NULL
        );
        "#,
    )
    .execute(pool)
    .await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_generated_created ON generated_images(created_at DESC)")
        .execute(pool)
        .await?;
    Ok(())
}

pub fn router(state: Arc<GalleryState>) -> Router {
    Router::new()
        .route("/generate", post(generate))
        .route("/images", get(list_images))
        .route("/images/{id}", axum::routing::delete(delete_image))
        .route("/status", get(status))
        .with_state(state)
}

#[derive(Serialize, sqlx::FromRow)]
struct ImageRow {
    id: String,
    prompt: String,
    seed: i64,
    width: i64,
    height: i64,
    steps: i64,
    created_at: String,
}

#[derive(Deserialize)]
struct GenerateReq {
    prompt: String,
    seed: Option<i64>,
    steps: Option<i64>,
    width: Option<i64>,
    height: Option<i64>,
}

/// Body forwarded to the Bonsai FastAPI backend (matches its GenerateRequest).
#[derive(Serialize)]
struct BonsaiReq {
    prompt: String,
    seed: i64,
    steps: i64,
    width: i64,
    height: i64,
}

async fn generate(
    State(s): State<Arc<GalleryState>>,
    Json(req): Json<GenerateReq>,
) -> Result<Json<ImageRow>, GalleryError> {
    let prompt = req.prompt.trim().to_string();
    if prompt.is_empty() {
        return Err(GalleryError::BadRequest("prompt is required".into()));
    }
    // Default to a time-derived seed so repeated identical prompts vary, unless
    // the caller pins one for reproducibility.
    let seed = req.seed.unwrap_or_else(|| {
        (Utc::now().timestamp_subsec_nanos() & 0x7fff_ffff) as i64
    });
    let steps = req.steps.unwrap_or(4).clamp(1, 50);
    let width = req.width.unwrap_or(512).clamp(64, 1536);
    let height = req.height.unwrap_or(512).clamp(64, 1536);

    let body = BonsaiReq { prompt: prompt.clone(), seed, steps, width, height };
    let resp = s
        .http
        .post(format!("{}/generate", s.bonsai_url))
        .json(&body)
        .send()
        .await
        .map_err(|e| GalleryError::Backend(format!("bonsai unreachable: {e}")))?;
    if !resp.status().is_success() {
        let code = resp.status();
        let detail = resp.text().await.unwrap_or_default();
        return Err(GalleryError::Backend(format!(
            "bonsai returned {code}: {}",
            detail.chars().take(300).collect::<String>()
        )));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| GalleryError::Backend(format!("reading bonsai PNG: {e}")))?;

    let id = uuid::Uuid::new_v4().to_string();
    tokio::fs::create_dir_all(&s.files_dir)
        .await
        .map_err(|e| GalleryError::Io(format!("create gallery dir: {e}")))?;
    let path = s.files_dir.join(format!("{id}.png"));
    tokio::fs::write(&path, &bytes)
        .await
        .map_err(|e| GalleryError::Io(format!("write png: {e}")))?;

    let created_at = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO generated_images (id, prompt, seed, width, height, steps, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(&id)
    .bind(&prompt)
    .bind(seed)
    .bind(width)
    .bind(height)
    .bind(steps)
    .bind(&created_at)
    .execute(&s.pool)
    .await?;

    Ok(Json(ImageRow {
        id,
        prompt,
        seed,
        width,
        height,
        steps,
        created_at,
    }))
}

async fn list_images(
    State(s): State<Arc<GalleryState>>,
) -> Result<Json<Vec<ImageRow>>, GalleryError> {
    let rows: Vec<ImageRow> = sqlx::query_as::<_, ImageRow>(
        "SELECT id, prompt, seed, width, height, steps, created_at
           FROM generated_images
          ORDER BY created_at DESC
          LIMIT 200",
    )
    .fetch_all(&s.pool)
    .await?;
    Ok(Json(rows))
}

async fn delete_image(
    State(s): State<Arc<GalleryState>>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, GalleryError> {
    let res = sqlx::query("DELETE FROM generated_images WHERE id = ?1")
        .bind(&id)
        .execute(&s.pool)
        .await?;
    if res.rows_affected() == 0 {
        return Err(GalleryError::NotFound);
    }
    // Best-effort file removal — a missing file shouldn't fail the delete.
    let _ = tokio::fs::remove_file(s.files_dir.join(format!("{id}.png"))).await;
    Ok(Json(serde_json::json!({ "ok": true, "id": id })))
}

#[derive(Serialize)]
struct StatusResp {
    reachable: bool,
    backend_url: String,
}

async fn status(State(s): State<Arc<GalleryState>>) -> Json<StatusResp> {
    // Quick reachability probe with a short timeout — the model backend exposes
    // GET /backends. Don't surface errors; the UI only needs up/down.
    let reachable = s
        .http
        .get(format!("{}/backends", s.bonsai_url))
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false);
    Json(StatusResp {
        reachable,
        backend_url: s.bonsai_url.clone(),
    })
}

#[derive(Debug)]
pub enum GalleryError {
    Sqlx(sqlx::Error),
    BadRequest(String),
    /// The Bonsai backend was unreachable or returned an error.
    Backend(String),
    Io(String),
    NotFound,
}

impl From<sqlx::Error> for GalleryError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sqlx(e)
    }
}

impl IntoResponse for GalleryError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            Self::Backend(m) => (StatusCode::BAD_GATEWAY, m),
            Self::Io(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
            Self::Sqlx(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}")),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}
