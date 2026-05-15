//! news-api — axum HTTP server for the news feed UI + upvote/downvote.
//! Served via Cloudflare tunnel at the URL set by `NUCLEUS_NEWS_PUBLIC_URL`.

use anyhow::{Context, Result};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json},
    routing::{get, post},
    Router,
};
use chrono::Utc;
use nucleus_core::{config::Settings, db};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::trace::TraceLayer;

const DB_PATH: &str = "memory/news.db";

#[derive(Clone)]
struct AppState {
    pool: SqlitePool,
    /// Index HTML rendered once at startup with the dashboard / chat tunnel
    /// URLs substituted in (or stripped to `display:none` if unset).
    index_html: Arc<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    nucleus_core::init_tracing();
    let settings = Settings::load().context("loading settings")?;
    let workspace_root = std::env::current_dir()?;
    let pool = db::open(&workspace_root.join(DB_PATH)).await?;
    let index_html = render_index_html(
        settings.public_urls.dashboard.as_deref(),
        settings.public_urls.chat.as_deref(),
    );
    let state = Arc::new(AppState { pool, index_html: Arc::new(index_html) });

    let app = Router::new()
        .route("/", get(index))
        .route("/favicon.svg", get(favicon))
        .route("/favicon-light.svg", get(favicon_light))
        .route("/favicon-dark.svg", get(favicon_dark))
        .route("/api/health", get(health))
        .route("/api/items", get(list_items))
        .route("/api/items/notable", get(list_notable))
        .route("/api/sources", get(list_sources))
        .route("/api/runs", get(list_runs))
        .route("/api/vote", post(vote))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let port = settings.ports.news_api;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    tracing::info!("news-api: listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(Serialize)]
struct HealthResp {
    status: &'static str,
    service: &'static str,
    now: String,
}

async fn health() -> Json<HealthResp> {
    Json(HealthResp {
        status: "ok",
        service: "news-api",
        now: Utc::now().to_rfc3339(),
    })
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
        self.fetch_date.clone()
            .or_else(|| self.day.clone())
            .unwrap_or_else(|| Utc::now().format("%Y-%m-%d").to_string())
    }
}

#[derive(Serialize, sqlx::FromRow)]
struct ItemDto {
    id: String,
    source_id: i64,
    source_name: String,
    url: String,
    /// Underlying article URL when `url` is a discussion page (HN, lobste.rs).
    /// Surfaced as an "↗ original" chip in the card / modal.
    article_url: Option<String>,
    title: String,
    summary: Option<String>,
    published_at: String,
    published_date: String,
    fetch_date: String,
    notable_score: Option<f64>,
    notable_reason: Option<String>,
    posted_to_discord: i64,
    upvotes: Option<i64>,
    downvotes: Option<i64>,
}

async fn list_items(State(s): State<Arc<AppState>>, Query(q): Query<ListQ>) -> Result<Json<Vec<ItemDto>>, ApiError> {
    let day = q.fetch_date_or_today();
    let min_score = q.min_score.unwrap_or(0.0);
    let limit = q.limit.unwrap_or(200).clamp(1, 500);
    let rows: Vec<ItemDto> = sqlx::query_as::<_, ItemDto>(r#"
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
    "#).bind(day).bind(min_score).bind(limit).fetch_all(&s.pool).await?;
    Ok(Json(rows))
}

async fn list_notable(State(s): State<Arc<AppState>>, Query(q): Query<ListQ>) -> Result<Json<Vec<ItemDto>>, ApiError> {
    let day = q.fetch_date_or_today();
    let limit = q.limit.unwrap_or(20).clamp(1, 100);
    let rows: Vec<ItemDto> = sqlx::query_as::<_, ItemDto>(r#"
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
    "#).bind(day).bind(limit).fetch_all(&s.pool).await?;
    Ok(Json(rows))
}

#[derive(Serialize, sqlx::FromRow)]
struct SourceDto {
    id: i64,
    name: String,
    url: String,
    enabled: i64,
    last_fetched_at: Option<String>,
    last_error: Option<String>,
}

async fn list_sources(State(s): State<Arc<AppState>>) -> Result<Json<Vec<SourceDto>>, ApiError> {
    let rows: Vec<SourceDto> = sqlx::query_as::<_, SourceDto>(
        "SELECT id, name, url, enabled, last_fetched_at, last_error FROM sources ORDER BY name"
    ).fetch_all(&s.pool).await?;
    Ok(Json(rows))
}

#[derive(Serialize, sqlx::FromRow)]
struct RunDto {
    run_id: String,
    started_at: String,
    finished_at: Option<String>,
    items_new: i64,
    items_notable: i64,
    ok: i64,
}

async fn list_runs(State(s): State<Arc<AppState>>) -> Result<Json<Vec<RunDto>>, ApiError> {
    let rows: Vec<RunDto> = sqlx::query_as::<_, RunDto>(
        "SELECT run_id, started_at, finished_at, items_new, items_notable, ok FROM fetcher_runs ORDER BY started_at DESC LIMIT 30"
    ).fetch_all(&s.pool).await?;
    Ok(Json(rows))
}

#[derive(Deserialize)]
struct VoteReq {
    item_id: String,
    vote: i32,
}

async fn vote(State(s): State<Arc<AppState>>, Json(req): Json<VoteReq>) -> Result<Json<serde_json::Value>, ApiError> {
    let v = if req.vote > 0 { 1 } else if req.vote < 0 { -1 } else {
        return Err(ApiError::BadRequest("vote must be -1 or 1".into()));
    };
    let now = Utc::now().to_rfc3339();
    sqlx::query("INSERT INTO votes (item_id, vote, created_at) VALUES (?1, ?2, ?3)")
        .bind(&req.item_id).bind(v).bind(&now).execute(&s.pool).await?;
    Ok(Json(serde_json::json!({ "ok": true, "item_id": req.item_id, "vote": v })))
}

async fn index(State(s): State<Arc<AppState>>) -> Html<String> {
    Html(s.index_html.as_ref().clone())
}

const INDEX_TEMPLATE: &str = include_str!("news_index.html");

/// Substitute the {{DASHBOARD_URL}} / {{CHAT_URL}} placeholders (and their
/// display siblings). Each link is hidden if its public URL isn't set so we
/// never render a half-broken nav item.
fn render_index_html(dashboard_url: Option<&str>, chat_url: Option<&str>) -> String {
    let mut html = INDEX_TEMPLATE.to_string();
    let (dash_href, dash_display) = match dashboard_url {
        Some(u) => (u.trim_end_matches('/').to_string(), "inline"),
        None => ("#".into(), "none"),
    };
    let (chat_href, chat_display) = match chat_url {
        Some(u) => (u.trim_end_matches('/').to_string(), "inline"),
        None => ("#".into(), "none"),
    };
    html = html
        .replace("{{DASHBOARD_URL}}", &dash_href)
        .replace("{{DASHBOARD_LINK_DISPLAY}}", dash_display)
        .replace("{{CHAT_URL}}", &chat_href)
        .replace("{{CHAT_LINK_DISPLAY}}", chat_display);
    html
}

const FAVICON_SVG: &str = include_str!("../../../assets/icons/news/02-rss-arcs.svg");
const FAVICON_LIGHT: &str = include_str!("../../../assets/icons/news/02-rss-arcs-light.svg");
const FAVICON_DARK: &str = include_str!("../../../assets/icons/news/02-rss-arcs-dark.svg");

async fn favicon() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], FAVICON_SVG)
}

async fn favicon_light() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], FAVICON_LIGHT)
}

async fn favicon_dark() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], FAVICON_DARK)
}


#[derive(Debug)]
enum ApiError {
    Sqlx(sqlx::Error),
    BadRequest(String),
}

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self { Self::Sqlx(e) }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::Sqlx(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {}", e)),
            Self::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}
