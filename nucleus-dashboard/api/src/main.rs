//! nucleus-dashboard — unified operator app (ADR-015).
//!
//! Single axum binary subsuming the standalone `dashboard/`, `chat/`,
//! and `news/api/` crates. Serves the React SPA shell + every operator
//! API surface + the chat WebSocket + the public news API at one origin.
//!
//! Routes are path-scoped; see ADR-015 §"Routes (axum)". The frontend
//! lives at `nucleus-dashboard/web/` (React + Vite + Tailwind v4),
//! built into `nucleus-dashboard/web/dist/` and served via tower-http
//! ServeDir at the root.

mod handlers;

use anyhow::{Context, Result};
use axum::{
    response::{Html, IntoResponse, Json},
    routing::get,
    Router,
};
use nucleus_core::{config::Settings, db};
use serde::Serialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::{services::ServeDir, trace::TraceLayer};

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
    version: &'static str,
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "nucleus-dashboard",
        version: env!("CARGO_PKG_VERSION"),
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    nucleus_core::init_tracing();
    let _settings = Settings::load().context("loading settings")?;
    let workspace_root = std::env::current_dir()?;

    // Vite build output. In dev, the Vite dev server runs separately and
    // proxies /api/* to this server; the ServeDir below is only used in
    // production-style serving from one origin.
    let web_dist: PathBuf = std::env::var("NUCLEUS_DASHBOARD_WEB_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .map(|p| p.join("web/dist"))
                .unwrap_or_else(|| PathBuf::from("web/dist"))
        });

    // Both DBs are tolerated-missing — if the news / reminders subsystem
    // hasn't been initialized yet on this machine the routes mount and
    // return 503s rather than crashing the whole binary.
    let news_pool = match db::open(&workspace_root.join("memory/news.db")).await {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!("nucleus-dashboard: news.db not openable: {}", e);
            None
        }
    };
    let reminders_pool = match db::open(&workspace_root.join("memory/reminders.db")).await {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!("nucleus-dashboard: reminders.db not openable: {}", e);
            None
        }
    };

    let infra_routes = Router::new().route("/health", get(health));

    let mut app = Router::new().nest("/api", infra_routes);

    if let Some(pool) = news_pool {
        let news_state = Arc::new(handlers::news::NewsState { pool });
        app = app.nest("/news/api", handlers::news::router(news_state));
    }

    // Cron router always mounts so the launchd-list endpoint works
    // even when reminders.db doesn't exist yet.
    let cron_state = Arc::new(handlers::cron::CronState {
        reminders_pool: reminders_pool.clone(),
    });
    app = app.nest("/cron/api", handlers::cron::router(cron_state));

    // SPA fallback — any path that ServeDir can't resolve (React Router
    // routes like /news, /chat) returns index.html with 200 so the
    // client-side router takes over. ServeDir's own not_found_service
    // preserves the 404 status which Playwright + browsers treat as a
    // failed navigation, so we do the fallback ourselves.
    //
    // index.html is re-read on every request: it's tiny (<1KB), the OS
    // caches the file in page cache so the disk hit is free, and it
    // saves us from a stale cached copy after `npm run build` swaps
    // the content-hashed asset filenames out from under a long-running
    // server. Production-fine; we serve at most one of these per
    // navigation.
    let index_html_path = Arc::new(web_dist.join("index.html"));

    let app = app
        .nest_service("/assets", ServeDir::new(web_dist.join("assets")))
        .fallback(move || {
            let path = index_html_path.clone();
            async move {
                match tokio::fs::read_to_string(path.as_ref()).await {
                    Ok(html) => Html(html).into_response(),
                    Err(e) => {
                        tracing::error!("nucleus-dashboard: reading SPA index.html: {}", e);
                        (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            "SPA shell not found",
                        )
                            .into_response()
                    }
                }
            }
        })
        .layer(TraceLayer::new_for_http());

    let port = _settings.ports.nucleus_dashboard.unwrap_or(8092);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    tracing::info!(
        "nucleus-dashboard listening on http://{} (serving SPA from {:?})",
        addr,
        web_dist
    );

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
