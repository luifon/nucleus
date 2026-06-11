//! News surface — public read API + admin views.
//!
//! Lifted from the standalone `news/api/` crate (ADR-015 §"Migration").
//! Routes mount under `/news/api/*` for the public contract (cloudflared
//! whitelist post-ADR-011) and `/news/api/admin/*` for operator-only
//! views like recent fetch runs and source health.

use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::sync::Arc;

#[derive(Clone)]
pub struct NewsState {
    pub pool: SqlitePool,
}

pub fn router(state: Arc<NewsState>) -> Router {
    Router::new()
        .route("/items", get(list_items))
        .route("/items/notable", get(list_notable))
        .route("/sources", get(list_sources))
        .route("/runs", get(list_runs))
        .route("/vote", post(vote))
        .with_state(state)
}

#[derive(Deserialize, Default)]
struct ListQ {
    /// Filter by the date items entered our DB (the "fetch bucket").
    /// Accepts `fetch_date` (preferred) or `day` (legacy alias).
    fetch_date: Option<String>,
    day: Option<String>,
    min_score: Option<f64>,
    limit: Option<i64>,
}

impl ListQ {
    fn fetch_date_or_today(&self) -> String {
        self.fetch_date
            .clone()
            .or_else(|| self.day.clone())
            .unwrap_or_else(|| Utc::now().format("%Y-%m-%d").to_string())
    }
}

#[derive(Serialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export)]
struct ItemDto {
    id: String,
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    source_id: i64,
    source_name: String,
    url: String,
    article_url: Option<String>,
    title: String,
    summary: Option<String>,
    published_at: String,
    published_date: String,
    fetch_date: String,
    notable_score: Option<f64>,
    notable_reason: Option<String>,
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    posted_to_discord: i64,
    #[ts(type = "number | null")]
    upvotes: Option<i64>,
    #[ts(type = "number | null")]
    downvotes: Option<i64>,
}

async fn list_items(
    State(s): State<Arc<NewsState>>,
    Query(q): Query<ListQ>,
) -> Result<Json<Vec<ItemDto>>, NewsError> {
    let day = q.fetch_date_or_today();
    let min_score = q.min_score.unwrap_or(0.0);
    let limit = q.limit.unwrap_or(200).clamp(1, 500);
    let rows: Vec<ItemDto> = sqlx::query_as::<_, ItemDto>(
        r#"
        SELECT i.id, i.source_id, s.name AS source_name,
               i.url, i.article_url,
               i.title, i.summary, i.published_at, i.published_date,
               i.fetch_date, i.notable_score, i.notable_reason, i.posted_to_discord,
               (SELECT COUNT(*) FROM votes v WHERE v.item_id = i.id AND v.vote =  1) AS upvotes,
               (SELECT COUNT(*) FROM votes v WHERE v.item_id = i.id AND v.vote = -1) AS downvotes
        FROM items i
        JOIN sources s ON s.id = i.source_id
        WHERE i.fetch_date = ?1
          AND COALESCE(i.notable_score, 0) >= ?2
        ORDER BY i.notable_score DESC NULLS LAST, i.published_at DESC
        LIMIT ?3
        "#,
    )
    .bind(day)
    .bind(min_score)
    .bind(limit)
    .fetch_all(&s.pool)
    .await?;
    Ok(Json(rows))
}

async fn list_notable(
    State(s): State<Arc<NewsState>>,
    Query(q): Query<ListQ>,
) -> Result<Json<Vec<ItemDto>>, NewsError> {
    let day = q.fetch_date_or_today();
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let rows: Vec<ItemDto> = sqlx::query_as::<_, ItemDto>(
        r#"
        SELECT i.id, i.source_id, s.name AS source_name,
               i.url, i.article_url,
               i.title, i.summary, i.published_at, i.published_date,
               i.fetch_date, i.notable_score, i.notable_reason, i.posted_to_discord,
               (SELECT COUNT(*) FROM votes v WHERE v.item_id = i.id AND v.vote =  1) AS upvotes,
               (SELECT COUNT(*) FROM votes v WHERE v.item_id = i.id AND v.vote = -1) AS downvotes
        FROM items i
        JOIN sources s ON s.id = i.source_id
        WHERE i.fetch_date = ?1
          AND COALESCE(i.notable_score, 0) >= 0.6
        ORDER BY i.notable_score DESC, i.published_at DESC
        LIMIT ?2
        "#,
    )
    .bind(day)
    .bind(limit)
    .fetch_all(&s.pool)
    .await?;
    Ok(Json(rows))
}

#[derive(Serialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export)]
struct SourceDto {
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    id: i64,
    name: String,
    url: String,
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    enabled: i64,
    last_fetched_at: Option<String>,
    last_error: Option<String>,
}

async fn list_sources(State(s): State<Arc<NewsState>>) -> Result<Json<Vec<SourceDto>>, NewsError> {
    let rows: Vec<SourceDto> = sqlx::query_as::<_, SourceDto>(
        "SELECT id, name, url, enabled, last_fetched_at, last_error FROM sources ORDER BY name",
    )
    .fetch_all(&s.pool)
    .await?;
    Ok(Json(rows))
}

#[derive(Serialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export)]
struct RunDto {
    run_id: String,
    started_at: String,
    finished_at: Option<String>,
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    items_new: i64,
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    items_notable: i64,
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    ok: i64,
}

async fn list_runs(State(s): State<Arc<NewsState>>) -> Result<Json<Vec<RunDto>>, NewsError> {
    let rows: Vec<RunDto> = sqlx::query_as::<_, RunDto>(
        "SELECT run_id, started_at, finished_at, items_new, items_notable, ok FROM fetcher_runs ORDER BY started_at DESC LIMIT 30",
    )
    .fetch_all(&s.pool)
    .await?;
    Ok(Json(rows))
}

#[derive(Deserialize, ts_rs::TS)]
#[ts(export)]
struct VoteReq {
    item_id: String,
    vote: i32,
}

async fn vote(
    State(s): State<Arc<NewsState>>,
    Json(req): Json<VoteReq>,
) -> Result<Json<serde_json::Value>, NewsError> {
    let v = if req.vote > 0 {
        1
    } else if req.vote < 0 {
        -1
    } else {
        return Err(NewsError::BadRequest("vote must be -1 or 1".into()));
    };
    let now = Utc::now().to_rfc3339();
    sqlx::query("INSERT INTO votes (item_id, vote, created_at) VALUES (?1, ?2, ?3)")
        .bind(&req.item_id)
        .bind(v)
        .bind(&now)
        .execute(&s.pool)
        .await?;
    Ok(Json(
        serde_json::json!({ "ok": true, "item_id": req.item_id, "vote": v }),
    ))
}

#[derive(Debug)]
pub enum NewsError {
    Sqlx(sqlx::Error),
    BadRequest(String),
}

impl From<sqlx::Error> for NewsError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sqlx(e)
    }
}

impl IntoResponse for NewsError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::Sqlx(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("db: {}", e),
            ),
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}
