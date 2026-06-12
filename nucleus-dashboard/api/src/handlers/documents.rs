//! Documents surface (ADR-018) — read-only viewer over the local document
//! library. The library is OWNED by the whatsapp package (TS writes
//! memory/documents.db + memory/documents/); this handler opens the DB
//! read-only (`db::open_read_only`, never `db::open` — create_if_missing
//! would conjure an empty foreign-owned DB) and exposes:
//!
//!   GET /documents/api/list   — active documents, newest first
//!   GET /documents/api/audit  — recent library events (LEFT JOIN names)
//!
//! No mutation endpoints — the docstore owns all writes. Deliberately,
//! dashboard views do NOT bump retrieve_count: only WhatsApp deliveries
//! count as retrievals (the manifest's "retrieved" column means "sent to
//! the operator", not "looked at").
//!
//! The file bytes are served by the /documents/files ServeDir mount in
//! main.rs with Cache-Control: no-store (identity documents must never
//! persist in a browser cache) — see the ADR-018 consequences section.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use serde::Serialize;
use sqlx::SqlitePool;
use std::sync::Arc;

pub struct DocumentsState {
    pub pool: SqlitePool,
}

pub fn router(state: Arc<DocumentsState>) -> Router {
    Router::new()
        .route("/list", get(list))
        .route("/audit", get(audit))
        .with_state(state)
}

#[derive(Serialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export)]
struct DocumentRow {
    id: String,
    logical_name: String,
    /// JSON array string as stored; the UI parses tolerantly.
    tags: String,
    filename: String,
    ext: String,
    mimetype: String,
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    bytes: i64,
    source: String,
    added_at: String,
    last_retrieved_at: Option<String>,
    #[ts(type = "number")]
    retrieve_count: i64,
}

#[derive(Serialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export)]
struct DocumentAuditRow {
    doc_id: String,
    /// NULL when the document was hard-removed from the table (shouldn't
    /// happen — deletes are soft — but the LEFT JOIN is honest).
    logical_name: Option<String>,
    action: String,
    channel: String,
    detail: Option<String>,
    at: String,
}

async fn list(
    State(s): State<Arc<DocumentsState>>,
) -> Result<Json<Vec<DocumentRow>>, DocumentsError> {
    let rows: Vec<DocumentRow> = sqlx::query_as(
        "SELECT id, logical_name, tags, filename, ext, mimetype, bytes,
                source, added_at, last_retrieved_at, retrieve_count
           FROM documents
          WHERE status = 'active'
          ORDER BY added_at DESC
          LIMIT 500",
    )
    .fetch_all(&s.pool)
    .await?;
    Ok(Json(rows))
}

async fn audit(
    State(s): State<Arc<DocumentsState>>,
) -> Result<Json<Vec<DocumentAuditRow>>, DocumentsError> {
    let rows: Vec<DocumentAuditRow> = sqlx::query_as(
        "SELECT a.doc_id, d.logical_name, a.action, a.channel, a.detail, a.at
           FROM doc_audit a
           LEFT JOIN documents d ON d.id = a.doc_id
          ORDER BY a.at DESC, a.id DESC
          LIMIT 200",
    )
    .fetch_all(&s.pool)
    .await?;
    Ok(Json(rows))
}

#[derive(Debug)]
pub enum DocumentsError {
    Sqlx(sqlx::Error),
}

impl From<sqlx::Error> for DocumentsError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sqlx(e)
    }
}

impl IntoResponse for DocumentsError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::Sqlx(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}")),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}
