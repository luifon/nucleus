//! Typed settings.
//!
//! Two sources, with clear separation:
//! - **`.env`** (gitignored) — anything personally identifying: user names,
//!   workspace paths, channel/user IDs, tokens, allowlists. Loaded via dotenvy.
//! - **`nucleus.toml`** (commit-safe) — non-identifying tunables: cron
//!   schedules, retention windows, ports, denylists, permission mode.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Settings {
    /// Personal identifiers — sourced from env, never persisted to a committed file.
    pub identity: Identity,
    pub public_urls: PublicUrls,
    pub claude: ClaudeConfig,
    pub discord: DiscordConfig,
    pub whatsapp: WhatsAppConfig,
    pub obsidian: ObsidianConfig,
    pub diary: DiaryConfig,
    pub distiller: DistillerConfig,
    pub news: NewsConfig,
    pub ports: PortsConfig,
}

/// Public-facing URLs for each tunnel-fronted service. All optional —
/// if a URL isn't set, that surface's tunnel health check is skipped and
/// any cross-link to it is hidden in the UI.
#[derive(Debug, Clone, Default)]
pub struct PublicUrls {
    pub news: Option<String>,
    pub dashboard: Option<String>,
    pub containers: Option<String>,
    pub chat: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Identity {
    pub user_name: String,
    pub workspace_root: PathBuf,
    pub tier2_dir: PathBuf,
    pub mem0_user_id: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClaudeConfig {
    pub binary: String,
    pub permission_mode: String,
    pub disallowed_tools: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DiscordConfig {
    /// From DISCORD_ALLOWED_USER_IDS (comma-separated).
    pub allowed_user_ids: Vec<String>,
    /// From DISCORD_HOME_CHANNEL_ID.
    pub home_channel_id: String,
    /// From [discord] table in nucleus.toml.
    pub mention_only_in_channels: bool,
    /// From [discord] table in nucleus.toml.
    pub dms_always_respond: bool,
}

#[derive(Debug, Clone)]
pub struct WhatsAppConfig {
    /// From WHATSAPP_ALLOWED_CHAT_IDS (comma-separated).
    pub allowed_chat_ids: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ObsidianConfig {
    pub vault_path: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DiaryConfig {
    pub root: String,
    pub retain_days: u32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DistillerConfig {
    pub metabolism_cron: String,
    pub contemplation_cron: String,
    pub metabolism_model: Option<String>,
    pub contemplation_model: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NewsConfig {
    pub fetch_cron: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PortsConfig {
    pub news_api: u16,
    pub dashboard: u16,
    pub chat: u16,
}

// Intermediate struct for what we read from nucleus.toml.
#[derive(Debug, Deserialize)]
struct TomlConfig {
    claude: ClaudeConfig,
    discord: TomlDiscord,
    obsidian: ObsidianConfig,
    diary: DiaryConfig,
    distiller: DistillerConfig,
    news: NewsConfig,
    ports: PortsConfig,
}

#[derive(Debug, Deserialize)]
struct TomlDiscord {
    mention_only_in_channels: bool,
    dms_always_respond: bool,
}

impl Settings {
    pub fn load() -> Result<Self> {
        use figment::providers::Format;
        let _ = dotenvy::dotenv();

        let toml: TomlConfig = figment::Figment::new()
            .merge(figment::providers::Toml::file("nucleus.toml"))
            .extract()
            .context("loading nucleus.toml")?;

        let identity = Identity {
            user_name: env_required("NUCLEUS_USER_NAME")?,
            workspace_root: PathBuf::from(env_required("NUCLEUS_WORKSPACE_ROOT")?),
            tier2_dir: PathBuf::from(env_required("NUCLEUS_TIER2_DIR")?),
            mem0_user_id: std::env::var("MEM0_USER_ID").unwrap_or_else(|_| "user".into()),
        };

        let discord = DiscordConfig {
            allowed_user_ids: split_csv(&std::env::var("DISCORD_ALLOWED_USER_IDS").unwrap_or_default()),
            home_channel_id: std::env::var("DISCORD_HOME_CHANNEL_ID").unwrap_or_default(),
            mention_only_in_channels: toml.discord.mention_only_in_channels,
            dms_always_respond: toml.discord.dms_always_respond,
        };

        let whatsapp = WhatsAppConfig {
            allowed_chat_ids: split_csv(&std::env::var("WHATSAPP_ALLOWED_CHAT_IDS").unwrap_or_default()),
        };

        let public_urls = PublicUrls {
            news: env_optional("NUCLEUS_NEWS_PUBLIC_URL"),
            dashboard: env_optional("NUCLEUS_DASHBOARD_PUBLIC_URL"),
            containers: env_optional("NUCLEUS_CONTAINERS_PUBLIC_URL"),
            chat: env_optional("NUCLEUS_CHAT_PUBLIC_URL"),
        };

        Ok(Settings {
            identity,
            public_urls,
            claude: toml.claude,
            discord,
            whatsapp,
            obsidian: toml.obsidian,
            diary: toml.diary,
            distiller: toml.distiller,
            news: toml.news,
            ports: toml.ports,
        })
    }
}

fn env_optional(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn env_required(key: &str) -> Result<String> {
    std::env::var(key)
        .with_context(|| format!("required env var `{}` is not set (see .env.example)", key))
}

fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Substitute `${USER_NAME}` placeholders in a string with the configured name.
pub fn substitute(s: &str, identity: &Identity) -> String {
    s.replace("${USER_NAME}", &identity.user_name)
}
