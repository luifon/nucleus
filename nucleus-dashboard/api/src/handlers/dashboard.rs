//! Dashboard widgets for the `/` landing. Glance-and-route hub: small
//! health overview + activity glances + docker + tunnel probe.
//!
//! Deliberately does NOT include a unified agents-health surface — that
//! waits for the ADR-016 agent registry, which gives us one canonical
//! list to read from instead of the launchctl+tmux ad-hoc model.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use serde::Serialize;
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::Command;

#[derive(Clone)]
pub struct DashboardState {
    pub workspace_root: PathBuf,
    pub vault_path: PathBuf,
    pub diary_root: PathBuf,
    pub news_pool: Option<SqlitePool>,
    pub reminders_pool: Option<SqlitePool>,
    pub chat_pool: Option<SqlitePool>,
    /// Public news URL for the cloudflared probe. None = skip tunnel
    /// tile entirely.
    pub tunnel_probe_url: Option<String>,
}

pub fn router(state: Arc<DashboardState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/glances", get(glances))
        .route("/docker", get(docker))
        .route("/tunnel", get(tunnel))
        .with_state(state)
}

// ─── health overview ────────────────────────────────────────────────────────

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct HealthCheck {
    name: &'static str,
    ok: bool,
    detail: String,
}

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct HealthOverview {
    checks: Vec<HealthCheck>,
    ok_count: usize,
    total: usize,
}

async fn health(State(s): State<Arc<DashboardState>>) -> Json<HealthOverview> {
    let mut checks = Vec::new();

    checks.push(check_path("vault", &s.vault_path));
    checks.push(check_path("diary", &s.diary_root));
    checks.push(check_db("news.db", &s.news_pool).await);
    checks.push(check_db("reminders.db", &s.reminders_pool).await);
    checks.push(check_db("chat.db", &s.chat_pool).await);
    checks.push(check_tmux().await);

    let ok_count = checks.iter().filter(|c| c.ok).count();
    let total = checks.len();
    Json(HealthOverview { checks, ok_count, total })
}

fn check_path(name: &'static str, p: &std::path::Path) -> HealthCheck {
    match std::fs::metadata(p) {
        Ok(m) if m.is_dir() => HealthCheck {
            name,
            ok: true,
            detail: p.display().to_string(),
        },
        Ok(_) => HealthCheck {
            name,
            ok: false,
            detail: format!("{} exists but isn't a directory", p.display()),
        },
        Err(e) => HealthCheck {
            name,
            ok: false,
            detail: format!("{}: {}", p.display(), e),
        },
    }
}

async fn check_db(name: &'static str, pool: &Option<SqlitePool>) -> HealthCheck {
    match pool {
        None => HealthCheck {
            name,
            ok: false,
            detail: "not opened at startup".into(),
        },
        Some(p) => match sqlx::query_scalar::<_, i64>("SELECT 1").fetch_one(p).await {
            Ok(_) => HealthCheck {
                name,
                ok: true,
                detail: "reachable".into(),
            },
            Err(e) => HealthCheck {
                name,
                ok: false,
                detail: e.to_string(),
            },
        },
    }
}

async fn check_tmux() -> HealthCheck {
    match Command::new("tmux").args(["list-sessions"]).output().await {
        Ok(out) if out.status.success() => {
            let count = String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| l.starts_with("nucleus-"))
                .count();
            HealthCheck {
                name: "tmux",
                ok: true,
                detail: format!("{} nucleus-* sessions", count),
            }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let ok = stderr.contains("no server running");
            HealthCheck {
                name: "tmux",
                ok,
                detail: if ok { "no sessions yet".into() } else { stderr.into_owned() },
            }
        }
        Err(e) => HealthCheck {
            name: "tmux",
            ok: false,
            detail: format!("spawn tmux: {}", e),
        },
    }
}

// ─── glances ────────────────────────────────────────────────────────────────

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct Glances {
    next_fire: Option<NextFireGlance>,
    latest_vault: Option<VaultGlance>,
    latest_diary: Option<DiaryGlance>,
    top_news: Option<NewsGlance>,
    latest_chat: Option<ChatGlance>,
}

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct NextFireGlance {
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    id: i64,
    title_or_body: String,
    next_fire_at: String,
    channels: Option<String>,
}

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct VaultGlance {
    relpath: String,
    bucket: String,
    // JSON numbers, not bigint — values fit f64 (ADR-020 typegen)
    #[ts(type = "number")]
    mtime_unix: i64,
}

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct DiaryGlance {
    agent: String,
    date: String,
    first_section: Option<String>,
}

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct NewsGlance {
    title: String,
    source_name: String,
    url: String,
    notable_score: Option<f64>,
}

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct ChatGlance {
    id: String,
    title: Option<String>,
    last_active: String,
}

async fn glances(State(s): State<Arc<DashboardState>>) -> Json<Glances> {
    Json(Glances {
        next_fire: glance_next_fire(&s.reminders_pool).await,
        latest_vault: glance_latest_vault(&s.vault_path).await,
        latest_diary: glance_latest_diary(&s.diary_root).await,
        top_news: glance_top_news(&s.news_pool).await,
        latest_chat: glance_latest_chat(&s.chat_pool).await,
    })
}

async fn glance_next_fire(pool: &Option<SqlitePool>) -> Option<NextFireGlance> {
    let pool = pool.as_ref()?;
    sqlx::query_as::<_, (i64, Option<String>, Option<String>, String, Option<String>, Option<String>)>(
        "SELECT r.id, r.title, r.body, r.next_fire_at, r.system_prompt,
                (SELECT GROUP_CONCAT(channel, ' | ')
                   FROM reminder_channels c WHERE c.reminder_id = r.id)
           FROM reminders r
          WHERE r.status IN ('active', 'pending') AND r.next_fire_at IS NOT NULL
          ORDER BY r.next_fire_at ASC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .map(|(id, title, body, next_fire_at, system_prompt, channels)| {
        let display = title
            .filter(|t| !t.trim().is_empty())
            .or(body.filter(|b| !b.trim().is_empty()))
            .or_else(|| system_prompt.map(|s| truncate(&s, 60)))
            .unwrap_or_else(|| "—".into());
        NextFireGlance {
            id,
            title_or_body: display,
            next_fire_at,
            channels,
        }
    })
}

async fn glance_latest_vault(vault: &std::path::Path) -> Option<VaultGlance> {
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    let mut stack = vec![vault.to_path_buf()];
    while let Some(d) = stack.pop() {
        let mut entries = match tokio::fs::read_dir(&d).await {
            Ok(e) => e,
            Err(_) => continue,
        };
        while let Ok(Some(dirent)) = entries.next_entry().await {
            let path = dirent.path();
            let name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };
            if name.starts_with('.') || name.starts_with('_') || name == "Home.md" {
                continue;
            }
            let ft = match dirent.file_type().await {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                stack.push(path);
                continue;
            }
            if !name.ends_with(".md") {
                continue;
            }
            if let Ok(meta) = dirent.metadata().await {
                if let Ok(mtime) = meta.modified() {
                    best = match best {
                        Some((_, bt)) if mtime <= bt => best,
                        _ => Some((path, mtime)),
                    };
                }
            }
        }
    }
    let (path, mtime) = best?;
    let relpath = path
        .strip_prefix(vault)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned());
    let bucket = relpath
        .split('/')
        .next()
        .filter(|s| s.contains('-'))
        .unwrap_or("")
        .to_string();
    let mtime_unix = mtime
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Some(VaultGlance { relpath, bucket, mtime_unix })
}

async fn glance_latest_diary(diary_root: &std::path::Path) -> Option<DiaryGlance> {
    let mut best: Option<(String, String, std::path::PathBuf)> = None;
    let mut entries = tokio::fs::read_dir(diary_root).await.ok()?;
    while let Ok(Some(dirent)) = entries.next_entry().await {
        let agent_dir = dirent.path();
        if !dirent.file_type().await.map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let agent = agent_dir.file_name()?.to_str()?.to_string();
        let mut latest: Option<String> = None;
        let mut entries = tokio::fs::read_dir(&agent_dir).await.ok()?;
        while let Ok(Some(d)) = entries.next_entry().await {
            let name = d.file_name().into_string().ok()?;
            let stem = name.strip_suffix(".md")?;
            if stem.starts_with('_') {
                continue;
            }
            if latest.as_deref().is_none_or(|cur| stem > cur) {
                latest = Some(stem.to_string());
            }
        }
        if let Some(date) = latest {
            if best.as_ref().is_none_or(|(_, bd, _)| date > *bd) {
                let path = agent_dir.join(format!("{date}.md"));
                best = Some((agent, date, path));
            }
        }
    }
    let (agent, date, path) = best?;
    let first_section = tokio::fs::read_to_string(&path).await.ok().and_then(|content| {
        content
            .lines()
            .skip_while(|l| !l.starts_with("## "))
            .next()
            .map(|s| s.trim_start_matches("## ").to_string())
    });
    Some(DiaryGlance { agent, date, first_section })
}

async fn glance_top_news(pool: &Option<SqlitePool>) -> Option<NewsGlance> {
    let pool = pool.as_ref()?;
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    sqlx::query_as::<_, (String, String, String, Option<f64>)>(
        "SELECT i.title, s.name, i.url, i.notable_score
           FROM items i JOIN sources s ON s.id = i.source_id
          WHERE i.fetch_date = ?1
          ORDER BY i.notable_score DESC NULLS LAST, i.published_at DESC
          LIMIT 1",
    )
    .bind(&today)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .map(|(title, source_name, url, notable_score)| NewsGlance {
        title,
        source_name,
        url,
        notable_score,
    })
}

async fn glance_latest_chat(pool: &Option<SqlitePool>) -> Option<ChatGlance> {
    let pool = pool.as_ref()?;
    sqlx::query_as::<_, (String, Option<String>, String)>(
        "SELECT id, title, last_active FROM obsidian_chats
          ORDER BY last_active DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .map(|(id, title, last_active)| ChatGlance { id, title, last_active })
}

fn truncate(s: &str, n: usize) -> String {
    let flat = s.trim().lines().next().unwrap_or("");
    if flat.chars().count() <= n {
        flat.to_string()
    } else {
        let mut out: String = flat.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

// ─── docker ─────────────────────────────────────────────────────────────────

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct DockerContainer {
    id: String,
    names: Vec<String>,
    image: String,
    state: String,
    status: String,
}

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct DockerResp {
    available: bool,
    error: Option<String>,
    containers: Vec<DockerContainer>,
}

async fn docker() -> Json<DockerResp> {
    let docker = match bollard::Docker::connect_with_local_defaults() {
        Ok(d) => d,
        Err(e) => {
            return Json(DockerResp {
                available: false,
                error: Some(e.to_string()),
                containers: Vec::new(),
            });
        }
    };
    let opts = bollard::container::ListContainersOptions::<String> {
        all: true,
        ..Default::default()
    };
    match docker.list_containers(Some(opts)).await {
        Ok(list) => Json(DockerResp {
            available: true,
            error: None,
            containers: list
                .into_iter()
                .map(|c| DockerContainer {
                    id: c.id.unwrap_or_default(),
                    names: c.names.unwrap_or_default(),
                    image: c.image.unwrap_or_default(),
                    state: c.state.unwrap_or_default(),
                    status: c.status.unwrap_or_default(),
                })
                .collect(),
        }),
        Err(e) => Json(DockerResp {
            available: false,
            error: Some(e.to_string()),
            containers: Vec::new(),
        }),
    }
}

// ─── tunnel ─────────────────────────────────────────────────────────────────

#[derive(Serialize, ts_rs::TS)]
#[ts(export)]
struct TunnelResp {
    configured: bool,
    url: Option<String>,
    ok: bool,
    status_code: Option<u16>,
    #[ts(type = "number | null")]
    elapsed_ms: Option<u128>,
    error: Option<String>,
}

async fn tunnel(State(s): State<Arc<DashboardState>>) -> Json<TunnelResp> {
    let url = match &s.tunnel_probe_url {
        Some(u) => u.clone(),
        None => {
            return Json(TunnelResp {
                configured: false,
                url: None,
                ok: false,
                status_code: None,
                elapsed_ms: None,
                error: None,
            });
        }
    };
    let probe = format!("{}/api/health", url.trim_end_matches('/'));
    let started = Instant::now();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build();
    let client = match client {
        Ok(c) => c,
        Err(e) => {
            return Json(TunnelResp {
                configured: true,
                url: Some(probe),
                ok: false,
                status_code: None,
                elapsed_ms: None,
                error: Some(e.to_string()),
            });
        }
    };
    match client.get(&probe).send().await {
        Ok(resp) => {
            let code = resp.status().as_u16();
            Json(TunnelResp {
                configured: true,
                url: Some(probe),
                ok: resp.status().is_success(),
                status_code: Some(code),
                elapsed_ms: Some(started.elapsed().as_millis()),
                error: None,
            })
        }
        Err(e) => Json(TunnelResp {
            configured: true,
            url: Some(probe),
            ok: false,
            status_code: None,
            elapsed_ms: Some(started.elapsed().as_millis()),
            error: Some(e.to_string()),
        }),
    }
}

// ─── error type (currently unused; kept for future) ─────────────────────────

#[allow(dead_code)]
#[derive(Debug)]
pub enum DashboardError {
    Io(String),
}

impl IntoResponse for DashboardError {
    fn into_response(self) -> axum::response::Response {
        let (code, msg) = match self {
            Self::Io(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}
