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
use nucleus_core::{claude::PermissionMode, claude_session, config::Settings, db};
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

    if let Some(pool) = news_pool.clone() {
        let news_state = Arc::new(handlers::news::NewsState { pool });
        app = app.nest("/news/api", handlers::news::router(news_state));
    }

    // Reminders admin — requires the DB. Mount only when openable.
    // (The retired /cron surface's upcoming + fire-history views were folded
    // in here; its launchd list is superseded by /agents. ADR-016.)
    if let Some(pool) = reminders_pool.clone() {
        let state = Arc::new(handlers::reminders::RemindersState { pool });
        app = app.nest("/reminders/api", handlers::reminders::router(state));
    }

    // Agents — the ADR-016 front door (supersedes the old /sessions tmux
    // inspector, which is deleted). Reads agents.toml and probes
    // liveness per agent. Tolerated-missing: if the registry can't load
    // the surface is simply absent rather than crashing the binary.
    match nucleus_core::agents::Registry::load_from(workspace_root.join("agents.toml")) {
        Ok(registry) => {
            let agents_state = Arc::new(handlers::agents::AgentsState {
                workspace_root: workspace_root.clone(),
                registry,
                identity: _settings.identity.clone(),
            });
            app = app.nest("/agents/api", handlers::agents::router(agents_state));
        }
        Err(e) => {
            tracing::warn!("nucleus-dashboard: agents.toml not loadable: {} — /agents disabled", e);
        }
    }

    // Vault — filesystem mtime feed over the Obsidian vault.
    // Tilde-expand the configured vault_path since the config loader
    // doesn't do it for us today.
    let vault_path_raw = &_settings.obsidian.vault_path;
    let vault_root = if let Some(rest) = vault_path_raw.strip_prefix("~/") {
        std::env::var("HOME")
            .map(|h| PathBuf::from(h).join(rest))
            .unwrap_or_else(|_| PathBuf::from(vault_path_raw))
    } else if vault_path_raw == "~" {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(vault_path_raw))
    } else {
        PathBuf::from(vault_path_raw)
    };
    let vault_state = Arc::new(handlers::vault::VaultState {
        root: vault_root.clone(),
    });
    app = app.nest("/vault/api", handlers::vault::router(vault_state));

    let diary_root_for_dash = workspace_root.join(&_settings.diary.root);
    let chat_pool_for_dash = match db::open(&workspace_root.join("memory/chat.db")).await {
        Ok(p) => Some(p),
        Err(_) => None,
    };
    let dashboard_state = Arc::new(handlers::dashboard::DashboardState {
        workspace_root: workspace_root.clone(),
        vault_path: vault_root.clone(),
        diary_root: diary_root_for_dash,
        news_pool: news_pool.clone(),
        reminders_pool: reminders_pool.clone(),
        chat_pool: chat_pool_for_dash,
        tunnel_probe_url: _settings.public_urls.nucleus.clone(),
    });
    app = app.nest("/api/dashboard", handlers::dashboard::router(dashboard_state));

    match init_chat(&workspace_root, &_settings, &vault_root).await {
        Ok(state) => {
            let state = Arc::new(state);
            spawn_daily_rotation(state.clone());
            app = app.nest("/chat/api", handlers::chat::router(state));
        }
        Err(e) => {
            tracing::warn!("nucleus-dashboard: chat init failed: {} — surface disabled", e);
        }
    }

    // Skills router — walks both skill trees. Operator tier resolves
    // to $HOME/.claude/skills/; repo tier is relative to the workspace
    // root. Both tolerated-missing.
    let operator_skills = std::env::var("HOME")
        .map(PathBuf::from)
        .map(|h| h.join(".claude/skills"))
        .unwrap_or_else(|_| PathBuf::from(".claude/skills"));
    let repo_skills = workspace_root.join(".claude/skills");
    let skills_state = Arc::new(handlers::skills::SkillsState {
        operator_root: operator_skills,
        repo_root: repo_skills,
    });
    app = app.nest("/skills/api", handlers::skills::router(skills_state));

    // Diary router — per ADR-004, every bot writes to
    // memory/diaries/<agent>/<YYYY-MM-DD>.md.
    let diary_root = workspace_root.join(&_settings.diary.root);
    let diary_state = Arc::new(handlers::diary::DiaryState { root: diary_root });
    app = app.nest("/diary/api", handlers::diary::router(diary_state));

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

    let port = _settings.ports.nucleus_dashboard;
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

async fn init_chat(
    workspace_root: &std::path::Path,
    settings: &Settings,
    vault_path: &std::path::Path,
) -> Result<handlers::chat::ChatState> {
    const CHAT_DB_PATH: &str = "memory/chat.db";

    // The one-time dashboard.db → chat.db migration is retired (ADR-016):
    // chat.db has long existed, so the `if !chat_db.exists()` guard never
    // fired, and dashboard.db's sole chat is already present in chat.db.
    let chat_db = workspace_root.join(CHAT_DB_PATH);
    let pool = db::open(&chat_db).await?;
    handlers::chat::ensure_schema(&pool).await?;

    let permission_mode = match PermissionMode::parse(&settings.claude.permission_mode) {
        Some(m) => Some(m),
        None => {
            tracing::warn!(
                mode = %settings.claude.permission_mode,
                "chat: unknown claude permission_mode — using default"
            );
            None
        }
    };

    let persona = nucleus_core::config::resolve_persona(&settings.identity, "chat", None)
        .context("resolving chat persona")?;
    let persona_display_name = persona.display_name.clone();

    let sessions = claude_session::SessionPool::new(claude_session::PoolConfig {
        workspace_root: workspace_root.to_path_buf(),
        append_system_prompt: Some(persona.body),
        permission_mode,
        disallowed_tools: settings.claude.disallowed_tools.clone(),
        allowed_tools: vec![],
        add_dirs: vec![vault_path.to_path_buf()],
        tmux_session: "nucleus-chat".into(),
        idle_timeout: std::time::Duration::from_secs(60 * 60 * 2),
        agent_label: Some("chat".into()),
        review_nudge_interval: if settings.skill_learner.enabled {
            settings.skill_learner.nudge_interval
        } else {
            0
        },
    });

    Ok(handlers::chat::ChatState {
        pool,
        persona_display_name,
        vault_path: vault_path.to_path_buf(),
        workspace_root: workspace_root.to_path_buf(),
        sessions,
    })
}

fn spawn_daily_rotation(state: Arc<handlers::chat::ChatState>) {
    tokio::spawn(async move {
        loop {
            nucleus_core::claude_session::sleep_until_next_4am().await;
            let pool = state.pool.clone();
            let stats = state
                .sessions
                .daily_rotate("chat", move |chat_key, new_session_id| {
                    let pool = pool.clone();
                    async move {
                        let now = chrono::Utc::now().to_rfc3339();
                        sqlx::query(
                            "UPDATE obsidian_chats \
                             SET claude_session_id = ?1, last_active = ?2 \
                             WHERE id = ?3",
                        )
                        .bind(&new_session_id)
                        .bind(&now)
                        .bind(&chat_key)
                        .execute(&pool)
                        .await
                        .map(|_| ())
                        .context("rotation: update obsidian_chats")
                    }
                })
                .await;
            tracing::info!(
                "chat: daily rotation — considered={} rotated={} skipped={} failed={}",
                stats.considered,
                stats.rotated,
                stats.skipped,
                stats.failed
            );
        }
    });
}
