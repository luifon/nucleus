//! chat — standalone HTTP service for the Obsidian chat UI, lifted out of
//! the dashboard crate so it can live behind its own tunnel
//! (`$NUCLEUS_CHAT_PUBLIC_URL`) and be deployed/restarted independently of
//! the dashboard.

mod api;

use anyhow::{Context, Result};
use axum::{
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use chrono::Utc;
use nucleus_core::{claude::PermissionMode, claude_session, config::Settings, db};
use serde::Serialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::trace::TraceLayer;

const CHAT_DB_PATH: &str = "memory/chat.db";
const LEGACY_DASHBOARD_DB: &str = "memory/dashboard.db";

#[tokio::main]
async fn main() -> Result<()> {
    nucleus_core::init_tracing();
    let settings = Settings::load().context("loading settings")?;
    let workspace_root = std::env::current_dir()?;

    // One-time migration: when the chat DB doesn't exist yet but the legacy
    // dashboard DB does (where the obsidian_chats / obsidian_messages tables
    // used to live), copy it over. Preserves chat history across the split.
    let chat_db = workspace_root.join(CHAT_DB_PATH);
    let legacy_db = workspace_root.join(LEGACY_DASHBOARD_DB);
    if !chat_db.exists() && legacy_db.exists() {
        if let Err(e) = std::fs::copy(&legacy_db, &chat_db) {
            tracing::warn!("chat: failed to migrate dashboard.db → chat.db: {e}");
        } else {
            tracing::info!("chat: migrated dashboard.db → chat.db (one-time copy)");
        }
    }

    let pool = db::open(&chat_db).await?;
    api::ensure_schema(&pool).await?;

    let permission_mode = match PermissionMode::parse(&settings.claude.permission_mode) {
        Some(m) => Some(m),
        None => {
            tracing::warn!(
                mode = %settings.claude.permission_mode,
                "chat: unknown claude permission_mode in config — using default"
            );
            None
        }
    };
    let vault_path = expand_home(&settings.obsidian.vault_path);

    // Tear down any leftover chat tmux session from a previous run.
    let _ = tokio::process::Command::new("tmux")
        .args(["kill-session", "-t", "nucleus-chat"])
        .output()
        .await;

    let sessions = claude_session::SessionPool::new(claude_session::PoolConfig {
        workspace_root: workspace_root.clone(),
        append_system_prompt: None,
        permission_mode,
        disallowed_tools: settings.claude.disallowed_tools.clone(),
        add_dirs: vec![vault_path.clone()],
        tmux_session: "nucleus-chat".into(),
        idle_timeout: std::time::Duration::from_secs(60 * 60 * 2),
    });

    let index_html = render_index_html(settings.public_urls.dashboard.as_deref());

    let chat_state = Arc::new(api::ChatState {
        pool,
        vault_path,
        workspace_root,
        sessions,
        index_html: Arc::new(index_html),
    });

    let app = Router::new()
        .route("/favicon.svg", get(favicon))
        .route("/favicon-light.svg", get(favicon_light))
        .route("/favicon-dark.svg", get(favicon_dark))
        .route("/api/health", get(health))
        .merge(api::router(chat_state))
        .layer(TraceLayer::new_for_http());

    let port = settings.ports.chat;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    tracing::info!("chat: listening on http://{}", addr);
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

async fn health() -> Json<HealthResp> {
    Json(HealthResp {
        status: "ok",
        service: "chat",
        now: Utc::now().to_rfc3339(),
    })
}

const INDEX_TEMPLATE: &str = include_str!("chat_index.html");

/// Substitute the {{DASHBOARD_URL}} / {{DASHBOARD_LINK_DISPLAY}} placeholders so
/// the "← dashboard" back-link points at the public tunnel when one is configured,
/// and is hidden gracefully when it isn't.
fn render_index_html(dashboard_url: Option<&str>) -> String {
    match dashboard_url {
        Some(url) => INDEX_TEMPLATE
            .replace("{{DASHBOARD_URL}}", url.trim_end_matches('/'))
            .replace("{{DASHBOARD_LINK_DISPLAY}}", "inline"),
        None => INDEX_TEMPLATE
            .replace("{{DASHBOARD_URL}}", "#")
            .replace("{{DASHBOARD_LINK_DISPLAY}}", "none"),
    }
}

const FAVICON_SVG: &str = include_str!("../../assets/icons/chat/01-prompt.svg");
const FAVICON_LIGHT: &str = include_str!("../../assets/icons/chat/01-prompt-light.svg");
const FAVICON_DARK: &str = include_str!("../../assets/icons/chat/01-prompt-dark.svg");

async fn favicon() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], FAVICON_SVG)
}
async fn favicon_light() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], FAVICON_LIGHT)
}
async fn favicon_dark() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], FAVICON_DARK)
}
