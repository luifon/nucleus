//! dashboard — local-services health + cross-links to the other Nucleus
//! surfaces (chat lives in its own crate now; we just link to it).
//! Served via a Cloudflare tunnel at the URL set by NUCLEUS_DASHBOARD_PUBLIC_URL.

mod collectors;

use anyhow::{Context, Result};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Json},
    routing::get,
    Router,
};
use chrono::Utc;
use nucleus_core::{config::Settings, db, health::Registry};
use serde::Serialize;
use sqlx::SqlitePool;
use std::net::SocketAddr;
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::RwLock;
use tower_http::trace::TraceLayer;

#[derive(Clone)]
struct AppState {
    registry: Arc<RwLock<Registry>>,
    index_html: Arc<String>,
    news_pool: Option<SqlitePool>,
    reminders_pool: Option<SqlitePool>,
    vault_path: Arc<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<()> {
    nucleus_core::init_tracing();
    let settings = Settings::load().context("loading settings")?;
    let workspace_root = std::env::current_dir()?;

    // Open news.db read-only-ish — passed to the fetcher health collector so it
    // can show last-run + counts. None if news pipeline isn't set up yet.
    let news_pool = match db::open(&workspace_root.join("memory/news.db")).await {
        Ok(p) => Some(p),
        Err(e) => { tracing::warn!("dashboard: news.db not openable: {}", e); None }
    };

    // Open reminders.db for the "pending reminders" widget. Same tolerance as
    // news.db — None if the user hasn't run the reminders CLI yet.
    let reminders_pool = match db::open(&workspace_root.join("memory/reminders.db")).await {
        Ok(p) => Some(p),
        Err(e) => { tracing::warn!("dashboard: reminders.db not openable: {}", e); None }
    };

    // Build collector registry. Tunnel checks only fire for URLs that are
    // actually configured — skipping unset ones keeps the dashboard from
    // showing perpetual "down" rows for tunnels you don't run.
    let mut registry = Registry::new();
    registry
        .register(collectors::self_check::SelfCheck)
        .register(collectors::docker::DockerCheck::new())
        .register(collectors::hermes::HermesCheck::new())
        .register(collectors::fetcher::FetcherCheck::new(news_pool.clone()));
    if let Some(url) = &settings.public_urls.news {
        registry.register(collectors::tunnel::TunnelCheck::new(
            "tunnel:news",
            format!("{}/api/health", url.trim_end_matches('/')),
        ));
    }
    if let Some(url) = &settings.public_urls.dashboard {
        registry.register(collectors::tunnel::TunnelCheck::new(
            "tunnel:dashboard",
            format!("{}/api/health", url.trim_end_matches('/')),
        ));
    }
    if let Some(url) = &settings.public_urls.containers {
        registry.register(collectors::tunnel::TunnelCheck::new(
            "tunnel:containers",
            format!("{}/health", url.trim_end_matches('/')),
        ));
    }

    // Render the index HTML once at startup with the news + chat URLs
    // substituted in (or stripped to a no-link state if unset).
    let index_html = render_index_html(
        settings.public_urls.news.as_deref(),
        settings.public_urls.chat.as_deref(),
    );

    let vault_path = expand_home(&settings.obsidian.vault_path);

    let app_state = AppState {
        registry: Arc::new(RwLock::new(registry)),
        index_html: Arc::new(index_html),
        news_pool,
        reminders_pool,
        vault_path: Arc::new(vault_path),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/favicon.svg", get(favicon))
        .route("/favicon-light.svg", get(favicon_light))
        .route("/favicon-dark.svg", get(favicon_dark))
        .route("/containers/{id}", get(container_detail_page))
        .route("/api/health", get(health_self))
        .route("/api/services", get(list_services))
        .route("/api/containers", get(list_containers))
        .route("/api/containers/{id}", get(container_detail_api))
        .route("/api/reminders", get(list_reminders))
        .route("/api/news/top", get(top_news))
        .route("/api/vault/recent", get(recent_vault_writes))
        .with_state(app_state)
        .layer(TraceLayer::new_for_http());

    let port = settings.ports.dashboard;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    tracing::info!("dashboard: listening on http://{}", addr);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(rest)
    } else {
        PathBuf::from(p)
    }
}

#[derive(Serialize)]
struct HealthResp {
    status: &'static str,
    service: &'static str,
    now: String,
}

async fn health_self() -> Json<HealthResp> {
    Json(HealthResp { status: "ok", service: "dashboard", now: Utc::now().to_rfc3339() })
}

async fn list_services(State(s): State<AppState>) -> Json<Vec<nucleus_core::health::Snapshot>> {
    let reg = s.registry.read().await;
    Json(reg.snapshot().await)
}

async fn list_containers(State(_s): State<AppState>) -> Json<serde_json::Value> {
    match collectors::docker::list_summaries().await {
        Ok(list) => Json(serde_json::json!({ "ok": true, "containers": list })),
        Err(e) => Json(serde_json::json!({ "ok": false, "error": e.to_string() })),
    }
}

async fn container_detail_api(Path(id): Path<String>) -> impl IntoResponse {
    match collectors::docker::detail(&id).await {
        Ok(d) => (StatusCode::OK, Json(serde_json::to_value(d).unwrap_or_default())).into_response(),
        Err(e) => (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": e.to_string() }))).into_response(),
    }
}

async fn container_detail_page(Path(_id): Path<String>) -> Html<&'static str> {
    Html(CONTAINER_DETAIL_HTML)
}

#[derive(Serialize, sqlx::FromRow)]
struct ReminderDto {
    id: i64,
    due_at: String,
    body: String,
    channel: String,
}

async fn list_reminders(State(s): State<AppState>) -> Json<serde_json::Value> {
    let Some(pool) = &s.reminders_pool else {
        return Json(serde_json::json!({ "ok": false, "items": [], "error": "reminders.db not available" }));
    };
    // Post-ADR-006: `due_at` is now `next_fire_at`, channels live in a
    // separate table, and one-shot vs recurring split into `pending`
    // and `active` statuses. We alias back to the wire shape the HTML
    // expects (`due_at`, `channel`) so the renderer doesn't change.
    // Skill-fire reminders (ADR-008) store an empty body + a
    // `system_prompt` — surface the prompt with a 🪄 marker so the
    // widget isn't blank, matching the CLI's `reminders list` output.
    let rows: Result<Vec<ReminderDto>, _> = sqlx::query_as::<_, ReminderDto>(
        "SELECT r.id,
                r.next_fire_at AS due_at,
                CASE
                    WHEN r.system_prompt IS NOT NULL AND r.system_prompt <> ''
                        THEN '🪄 ' || r.system_prompt
                    ELSE r.body
                END AS body,
                COALESCE(
                    (SELECT GROUP_CONCAT(rc.channel, ',')
                       FROM reminder_channels rc
                      WHERE rc.reminder_id = r.id),
                    ''
                ) AS channel
           FROM reminders r
          WHERE r.status IN ('pending', 'active')
            AND r.next_fire_at IS NOT NULL
          ORDER BY r.next_fire_at ASC
          LIMIT 20",
    )
    .fetch_all(pool)
    .await;
    match rows {
        Ok(items) => Json(serde_json::json!({ "ok": true, "items": items })),
        Err(e) => Json(serde_json::json!({ "ok": false, "items": [], "error": e.to_string() })),
    }
}

#[derive(Serialize, sqlx::FromRow)]
struct TopNewsDto {
    id: String,
    url: String,
    title: String,
    summary: Option<String>,
    published_date: String,
    fetch_date: String,
    notable_score: Option<f64>,
    notable_reason: Option<String>,
    source_name: String,
}

async fn top_news(State(s): State<AppState>) -> Json<serde_json::Value> {
    let Some(pool) = &s.news_pool else {
        return Json(serde_json::json!({ "ok": false, "items": [], "error": "news.db not available" }));
    };
    // Pull from the last 2 fetch buckets so the widget is never empty if today's
    // fetch hasn't produced notables yet. Order by score, then recency.
    let rows: Result<Vec<TopNewsDto>, _> = sqlx::query_as::<_, TopNewsDto>(
        r#"
        SELECT i.id, i.url, i.title, i.summary, i.published_date, i.fetch_date,
               i.notable_score, i.notable_reason,
               s.name AS source_name
          FROM items i
          JOIN sources s ON s.id = i.source_id
         WHERE i.fetch_date >= date('now', '-1 day')
         ORDER BY i.notable_score DESC NULLS LAST,
                  i.published_at DESC
         LIMIT 10
        "#,
    )
    .fetch_all(pool)
    .await;
    match rows {
        Ok(items) => Json(serde_json::json!({ "ok": true, "items": items })),
        Err(e) => Json(serde_json::json!({ "ok": false, "items": [], "error": e.to_string() })),
    }
}

#[derive(Serialize)]
struct VaultFileDto {
    /// Path relative to the vault root.
    path: String,
    /// Modified-time as an RFC3339 string in UTC.
    mtime: String,
    /// Modified-time as unix seconds — clients use this for relative formatting.
    mtime_unix: i64,
}

async fn recent_vault_writes(State(s): State<AppState>) -> Json<serde_json::Value> {
    let vault = (*s.vault_path).clone();
    if !vault.exists() {
        return Json(serde_json::json!({ "ok": false, "items": [], "error": format!("vault not found: {}", vault.display()) }));
    }
    let result = tokio::task::spawn_blocking(move || {
        let mut out: Vec<(SystemTime, PathBuf)> = Vec::new();
        walk_vault(&vault, &vault, &mut out, 0);
        out.sort_by(|a, b| b.0.cmp(&a.0));
        out.into_iter()
            .take(15)
            .map(|(mtime, abs)| {
                let rel = abs.strip_prefix(&vault).unwrap_or(&abs).to_path_buf();
                let mtime_dt = chrono::DateTime::<chrono::Utc>::from(mtime);
                VaultFileDto {
                    path: rel.to_string_lossy().to_string(),
                    mtime: mtime_dt.to_rfc3339(),
                    mtime_unix: mtime_dt.timestamp(),
                }
            })
            .collect::<Vec<_>>()
    })
    .await;
    match result {
        Ok(items) => Json(serde_json::json!({ "ok": true, "items": items })),
        Err(e) => Json(serde_json::json!({ "ok": false, "items": [], "error": e.to_string() })),
    }
}

/// Recursive walk capped at 6 levels — guards against any pathological symlink
/// loop, but the PARA vault is never deeper than 4 in practice.
fn walk_vault(root: &StdPath, dir: &StdPath, out: &mut Vec<(SystemTime, PathBuf)>, depth: usize) {
    if depth > 6 { return; }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        // Skip dotfiles / dotdirs (.obsidian, .trash, .git, …) — they aren't
        // user-authored content and would flood the list with metadata churn.
        if path.file_name().map_or(false, |n| n.to_string_lossy().starts_with('.')) { continue; }
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_dir() {
            walk_vault(root, &path, out, depth + 1);
        } else if ft.is_file() && path.extension().map_or(false, |e| e == "md") {
            if let Ok(md) = entry.metadata() {
                if let Ok(mt) = md.modified() {
                    out.push((mt, path));
                }
            }
        }
    }
}

const CONTAINER_DETAIL_HTML: &str = include_str!("container_detail.html");

async fn index(State(s): State<AppState>) -> Html<String> {
    Html(s.index_html.as_ref().clone())
}

const INDEX_TEMPLATE: &str = include_str!("dashboard_index.html");

/// Substitute the {{NEWS_URL}} / {{CHAT_URL}} placeholders (and their
/// display siblings). Each link is hidden if its public URL isn't set so we
/// never render a half-broken nav item.
fn render_index_html(news_url: Option<&str>, chat_url: Option<&str>) -> String {
    let mut html = INDEX_TEMPLATE.to_string();
    let (news_href, news_display) = match news_url {
        Some(u) => (u.trim_end_matches('/').to_string(), "inline"),
        None => ("#".into(), "none"),
    };
    let (chat_href, chat_display) = match chat_url {
        Some(u) => (u.trim_end_matches('/').to_string(), "inline"),
        // Local fallback: when no public tunnel is set, point at the local chat port.
        None => ("http://127.0.0.1:8091".into(), "inline"),
    };
    html = html
        .replace("{{NEWS_URL}}", &news_href)
        .replace("{{NEWS_LINK_DISPLAY}}", news_display)
        .replace("{{CHAT_URL}}", &chat_href)
        .replace("{{CHAT_LINK_DISPLAY}}", chat_display);
    html
}

const FAVICON_SVG: &str = include_str!("../../assets/icons/dashboard/01-pulse.svg");
const FAVICON_LIGHT: &str = include_str!("../../assets/icons/dashboard/01-pulse-light.svg");
const FAVICON_DARK: &str = include_str!("../../assets/icons/dashboard/01-pulse-dark.svg");

async fn favicon() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], FAVICON_SVG)
}

async fn favicon_light() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], FAVICON_LIGHT)
}

async fn favicon_dark() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], FAVICON_DARK)
}
