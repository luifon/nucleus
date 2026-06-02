//! Image-generation (gallery) surface — ADR-019.
//!
//! Proxies prompts to a registry of local image-model backends (each a FastAPI
//! service exposing `POST /generate` → raw PNG bytes), persists each result as a
//! file under `memory/gallery/` plus a row in `memory/gallery.db`, and serves
//! the gallery list. PNG bytes are served by a `ServeDir` mount at
//! `/gallery/files/*` wired in `main.rs`.
//!
//! Generation is **asynchronous** (a slow model like NoobAI takes minutes — far
//! longer than a phone/proxy will hold an HTTP request, which would 'TypeError:
//! Load Failed' on iOS and, worse, get the dashboard handler cancelled mid-flight
//! so the result was discarded). So `/generate` inserts a `pending` row, spawns
//! the actual generation on a background task **decoupled from the request**, and
//! returns immediately. The task writes the PNG + flips the row to `ready`
//! (or `failed`) whether or not any client is still connected. The frontend
//! polls the list while anything is `pending`.

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
    /// Ordered name→base-URL registry of image-model backends, e.g.
    /// `[("bonsai","http://127.0.0.1:8093"), ("noobai","http://127.0.0.1:8094")]`.
    pub backends: Vec<(String, String)>,
    /// Model used when a request omits one.
    pub default_model: String,
    pub http: reqwest::Client,
}

impl GalleryState {
    fn backend_url(&self, model: &str) -> Option<&str> {
        self.backends
            .iter()
            .find(|(n, _)| n == model)
            .map(|(_, u)| u.as_str())
    }
}

/// Per-model generation defaults (steps, width, height) when the request omits
/// them. Bonsai is a 4-step distilled model at 512²; NoobAI is SDXL at 1024².
fn model_defaults(model: &str) -> (i64, i64, i64) {
    match model {
        "bonsai" => (4, 512, 512),
        _ => (20, 1024, 1024),
    }
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
          created_at TEXT    NOT NULL,
          model      TEXT    NOT NULL DEFAULT 'bonsai',
          status     TEXT    NOT NULL DEFAULT 'ready',
          error      TEXT
        );
        "#,
    )
    .execute(pool)
    .await?;
    // Idempotent migrations for DBs created before these columns (ADR-019).
    // Ignore the duplicate-column error on already-migrated DBs.
    for stmt in [
        "ALTER TABLE generated_images ADD COLUMN model TEXT NOT NULL DEFAULT 'bonsai'",
        "ALTER TABLE generated_images ADD COLUMN status TEXT NOT NULL DEFAULT 'ready'",
        "ALTER TABLE generated_images ADD COLUMN error TEXT",
    ] {
        let _ = sqlx::query(stmt).execute(pool).await;
    }
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
    model: String,
    status: String,
    error: Option<String>,
}

#[derive(Deserialize)]
struct GenerateReq {
    prompt: String,
    model: Option<String>,
    seed: Option<i64>,
    steps: Option<i64>,
    width: Option<i64>,
    height: Option<i64>,
}

/// Body forwarded to a model backend (all speak this same shape).
#[derive(Serialize)]
struct BackendReq {
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
    let model = req.model.unwrap_or_else(|| s.default_model.clone());
    let url = s
        .backend_url(&model)
        .ok_or_else(|| GalleryError::BadRequest(format!("unknown model: {model}")))?
        .to_string();

    let (def_steps, def_w, def_h) = model_defaults(&model);
    let seed = req
        .seed
        .unwrap_or_else(|| (Utc::now().timestamp_subsec_nanos() & 0x7fff_ffff) as i64);
    let steps = req.steps.unwrap_or(def_steps).clamp(1, 60);
    let width = req.width.unwrap_or(def_w).clamp(64, 1536);
    let height = req.height.unwrap_or(def_h).clamp(64, 1536);

    let id = uuid::Uuid::new_v4().to_string();
    let created_at = Utc::now().to_rfc3339();

    // Insert the pending row up front so it's persisted regardless of the client
    // connection's fate.
    sqlx::query(
        "INSERT INTO generated_images
           (id, prompt, seed, width, height, steps, created_at, model, status, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending', NULL)",
    )
    .bind(&id)
    .bind(&prompt)
    .bind(seed)
    .bind(width)
    .bind(height)
    .bind(steps)
    .bind(&created_at)
    .bind(&model)
    .execute(&s.pool)
    .await?;

    // Run the actual generation detached from this request — survives a client
    // disconnect (iOS ~60s timeout) and persists the result either way.
    let s2 = s.clone();
    let (id2, prompt2, model2) = (id.clone(), prompt.clone(), model.clone());
    tokio::spawn(async move {
        let body = BackendReq { prompt: prompt2, seed, steps, width, height };
        let outcome = run_generation(&s2, &url, &id2, &body).await;
        match outcome {
            Ok(()) => {
                let _ = sqlx::query(
                    "UPDATE generated_images SET status='ready', error=NULL WHERE id=?1",
                )
                .bind(&id2)
                .execute(&s2.pool)
                .await;
            }
            Err(e) => {
                tracing::warn!("gallery: {model2} generation {id2} failed: {e}");
                let _ = sqlx::query(
                    "UPDATE generated_images SET status='failed', error=?2 WHERE id=?1",
                )
                .bind(&id2)
                .bind(e)
                .execute(&s2.pool)
                .await;
            }
        }
    });

    Ok(Json(ImageRow {
        id,
        prompt,
        seed,
        width,
        height,
        steps,
        created_at,
        model,
        status: "pending".into(),
        error: None,
    }))
}

/// Proxy to the model backend and write the PNG. Returns a short error string on
/// failure (stored on the row). Does not touch DB status — the caller does.
async fn run_generation(
    s: &GalleryState,
    url: &str,
    id: &str,
    body: &BackendReq,
) -> Result<(), String> {
    let resp = s
        .http
        .post(format!("{url}/generate"))
        .json(body)
        .send()
        .await
        .map_err(|e| format!("backend unreachable: {e}"))?;
    if !resp.status().is_success() {
        let code = resp.status();
        let detail = resp.text().await.unwrap_or_default();
        return Err(format!(
            "backend {code}: {}",
            detail.chars().take(300).collect::<String>()
        ));
    }
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("reading PNG: {e}"))?;
    tokio::fs::create_dir_all(&s.files_dir)
        .await
        .map_err(|e| format!("create gallery dir: {e}"))?;
    tokio::fs::write(s.files_dir.join(format!("{id}.png")), &bytes)
        .await
        .map_err(|e| format!("write png: {e}"))?;
    Ok(())
}

async fn list_images(
    State(s): State<Arc<GalleryState>>,
) -> Result<Json<Vec<ImageRow>>, GalleryError> {
    let rows: Vec<ImageRow> = sqlx::query_as::<_, ImageRow>(
        "SELECT id, prompt, seed, width, height, steps, created_at, model, status, error
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
    let _ = tokio::fs::remove_file(s.files_dir.join(format!("{id}.png"))).await;
    Ok(Json(serde_json::json!({ "ok": true, "id": id })))
}

#[derive(Serialize)]
struct BackendStatus {
    name: String,
    reachable: bool,
}

#[derive(Serialize)]
struct StatusResp {
    backends: Vec<BackendStatus>,
    default_model: String,
}

async fn status(State(s): State<Arc<GalleryState>>) -> Json<StatusResp> {
    let mut backends = Vec::with_capacity(s.backends.len());
    for (name, url) in &s.backends {
        let reachable = s
            .http
            .get(format!("{url}/backends"))
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false);
        backends.push(BackendStatus { name: name.clone(), reachable });
    }
    Json(StatusResp { backends, default_model: s.default_model.clone() })
}

#[derive(Debug)]
pub enum GalleryError {
    Sqlx(sqlx::Error),
    BadRequest(String),
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
            Self::Sqlx(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}")),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}
