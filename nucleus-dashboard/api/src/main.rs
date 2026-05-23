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
    response::Json,
    routing::get,
    Router,
};
use nucleus_core::config::Settings;
use serde::Serialize;
use std::net::SocketAddr;
use std::path::PathBuf;
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
    let settings = Settings::load().context("loading settings")?;

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

    let api_routes = Router::new().route("/health", get(health));

    let app = Router::new()
        .nest("/api", api_routes)
        .fallback_service(ServeDir::new(&web_dist).append_index_html_on_directories(true))
        .layer(TraceLayer::new_for_http());

    let port = settings.ports.nucleus_dashboard.unwrap_or(8092);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    tracing::info!("nucleus-dashboard listening on http://{} (serving SPA from {:?})", addr, web_dist);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
