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
    pub gmail: GmailConfig,
    pub reminders: RemindersConfig,
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
pub struct GmailConfig {
    pub metabolism_cron: String,
    pub classifier_model: String,
    pub killlist_auto_promote_threshold: u32,
    pub calendar_default_duration_min: u32,
    /// The Gmail trash account JARVIS operates on. Sourced from
    /// `NUCLEUS_GMAIL_ACCOUNT`. Empty when unset; persona/prompt
    /// `${GMAIL_ACCOUNT}` substitutions become empty strings in that case.
    #[serde(default)]
    pub account: String,
    /// Personal email JARVIS adds as attendee on calendar events.
    /// Sourced from NUCLEUS_PERSONAL_EMAIL — empty when unset, in which
    /// case calendar deliveries fail fast at delivery time.
    #[serde(default)]
    pub personal_email: String,
}

/// Settings for the reminders subsystem (ADR-006 + ADR-008).
///
/// `default_channels` is the fallback for system-prompt reminders when
/// neither the stored prompt nor the per-reminder `--channels` flag
/// specifies where outer-error alerts should land. Body-based reminders
/// always use their own per-reminder channels and ignore this default.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RemindersConfig {
    #[serde(default = "default_reminder_channels")]
    pub default_channels: Vec<String>,
}

impl Default for RemindersConfig {
    fn default() -> Self {
        Self {
            default_channels: default_reminder_channels(),
        }
    }
}

fn default_reminder_channels() -> Vec<String> {
    vec!["discord-home".to_string()]
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PortsConfig {
    pub nucleus_dashboard: u16,
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
    gmail: GmailConfig,
    #[serde(default)]
    reminders: RemindersConfig,
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

        let mut gmail = toml.gmail;
        gmail.account = std::env::var("NUCLEUS_GMAIL_ACCOUNT").unwrap_or_default();
        gmail.personal_email = std::env::var("NUCLEUS_PERSONAL_EMAIL").unwrap_or_default();

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
            gmail,
            reminders: toml.reminders,
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

/// Substitute `${GMAIL_ACCOUNT}` placeholders in a string with the configured
/// Gmail trash account. Kept separate from [`substitute`] so callers who
/// don't depend on Gmail don't carry the surface area.
pub fn substitute_gmail(s: &str, gmail: &GmailConfig) -> String {
    s.replace("${GMAIL_ACCOUNT}", &gmail.account)
}

/// Resolved persona ready for spawn-time use. See [`resolve_persona`].
#[derive(Debug, Clone)]
pub struct PersonaContent {
    /// Markdown body (frontmatter stripped, `${USER_NAME}` substituted).
    /// Feed into `SpawnOptions::append_system_prompt`.
    pub body: String,
    /// Human-readable name from the file's frontmatter `display_name`, or
    /// the slug if frontmatter is absent. Surfaced in reply footers etc.
    pub display_name: String,
}

/// Resolve the persona for a conversational venue. See ADR-009.
///
/// Reads `NUCLEUS_PERSONA_<VENUE>` (and, if `context` is `Some`,
/// `NUCLEUS_PERSONA_<VENUE>_<CONTEXT>` first — ADR-005b extension),
/// loads `<workspace_root>/personas/<slug>.md`, parses optional YAML
/// frontmatter for `display_name`, strips frontmatter from the body,
/// applies `${USER_NAME}` substitution.
///
/// Missing env var or missing file is a hard error — no silent fallback,
/// per ADR-009 §"Spawn-time resolution".
pub fn resolve_persona(
    identity: &Identity,
    venue: &str,
    context: Option<&str>,
) -> Result<PersonaContent> {
    let venue_upper = venue.to_ascii_uppercase();
    let (env_key, slug) = match context {
        Some(ctx) => {
            let ctx_upper = ctx.to_ascii_uppercase();
            let scoped = format!("NUCLEUS_PERSONA_{venue_upper}_{ctx_upper}");
            match std::env::var(&scoped).ok().filter(|v| !v.trim().is_empty()) {
                Some(v) => (scoped, v),
                None => {
                    let venue_key = format!("NUCLEUS_PERSONA_{venue_upper}");
                    let v = std::env::var(&venue_key).with_context(|| {
                        format!(
                            "neither `{scoped}` nor `{venue_key}` is set; \
                             one is required to resolve a persona for venue `{venue}` \
                             (context `{ctx}`)"
                        )
                    })?;
                    (venue_key, v)
                }
            }
        }
        None => {
            let key = format!("NUCLEUS_PERSONA_{venue_upper}");
            let v = std::env::var(&key).with_context(|| {
                format!(
                    "required env var `{key}` is not set; \
                     define a persona slug for venue `{venue}` in .env (see ADR-009)"
                )
            })?;
            (key, v)
        }
    };

    let slug = slug.trim().to_string();
    if slug.is_empty() {
        anyhow::bail!("env var `{env_key}` is set but empty");
    }

    let path = identity
        .workspace_root
        .join("personas")
        .join(format!("{slug}.md"));
    let raw = std::fs::read_to_string(&path).with_context(|| {
        format!(
            "reading persona file {} (resolved from {env_key}={slug})",
            path.display()
        )
    })?;

    let (frontmatter, body_raw) = split_frontmatter(&raw);
    let display_name = frontmatter
        .and_then(|fm| extract_yaml_field(fm, "display_name"))
        .unwrap_or_else(|| slug.clone());
    let body = substitute(body_raw, identity);

    Ok(PersonaContent { body, display_name })
}

/// Splits a YAML frontmatter block off the start of a markdown string.
/// Returns `(Some(frontmatter_body), rest)` if the document opens with
/// `---\n...\n---\n`, or `(None, original)` otherwise. The body of the
/// frontmatter is returned without the delimiter lines; `rest` is the
/// document after the closing delimiter with leading whitespace trimmed.
fn split_frontmatter(s: &str) -> (Option<&str>, &str) {
    let trimmed = s.trim_start_matches('\u{feff}');
    let Some(after_open) = trimmed.strip_prefix("---\n").or_else(|| trimmed.strip_prefix("---\r\n")) else {
        return (None, s);
    };
    // Find the closing delimiter on its own line.
    let mut search_from = 0usize;
    while let Some(idx) = after_open[search_from..].find("\n---") {
        let abs = search_from + idx;
        let after = &after_open[abs + 4..];
        // Closing delimiter must be followed by end-of-string or newline.
        if after.is_empty() || after.starts_with('\n') || after.starts_with("\r\n") {
            let frontmatter = &after_open[..abs];
            let rest = after.trim_start_matches('\r').trim_start_matches('\n');
            return (Some(frontmatter), rest);
        }
        search_from = abs + 4;
    }
    (None, s)
}

/// Pulls a single scalar field out of a tiny YAML frontmatter — just the
/// shapes we ship (`display_name: foo`, with optional quotes). Not a full
/// YAML parser; the frontmatter contract is intentionally narrow.
fn extract_yaml_field(frontmatter: &str, field: &str) -> Option<String> {
    for line in frontmatter.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if k.trim() != field {
            continue;
        }
        let v = v.trim();
        let v = v
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| v.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')))
            .unwrap_or(v);
        if v.is_empty() {
            return None;
        }
        return Some(v.to_string());
    }
    None
}

#[cfg(test)]
mod persona_tests {
    use super::*;
    use std::sync::Mutex;

    // Env mutation isn't thread-safe; serialize the persona tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn ident(workspace: &std::path::Path) -> Identity {
        Identity {
            user_name: "Alice".into(),
            workspace_root: workspace.to_path_buf(),
            tier2_dir: workspace.to_path_buf(),
            mem0_user_id: "test".into(),
        }
    }

    fn tempdir() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "nucleus-persona-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(p.join("personas")).unwrap();
        p
    }

    #[test]
    fn resolves_with_frontmatter_display_name_and_substitutes_user_name() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempdir();
        std::fs::write(
            dir.join("personas/robot.md"),
            "---\ndisplay_name: ROBOT\n---\n\nHello ${USER_NAME}.\n",
        )
        .unwrap();
        std::env::set_var("NUCLEUS_PERSONA_DISCORD", "robot");
        let p = resolve_persona(&ident(&dir), "discord", None).unwrap();
        assert_eq!(p.display_name, "ROBOT");
        assert_eq!(p.body.trim(), "Hello Alice.");
        std::env::remove_var("NUCLEUS_PERSONA_DISCORD");
    }

    #[test]
    fn falls_back_to_slug_when_no_frontmatter() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempdir();
        std::fs::write(
            dir.join("personas/assistant.md"),
            "Just a body, no frontmatter.\n",
        )
        .unwrap();
        std::env::set_var("NUCLEUS_PERSONA_WHATSAPP", "assistant");
        let p = resolve_persona(&ident(&dir), "whatsapp", None).unwrap();
        assert_eq!(p.display_name, "assistant");
        assert_eq!(p.body.trim(), "Just a body, no frontmatter.");
        std::env::remove_var("NUCLEUS_PERSONA_WHATSAPP");
    }

    #[test]
    fn errors_when_env_var_missing() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("NUCLEUS_PERSONA_GMAIL");
        let dir = tempdir();
        let err = resolve_persona(&ident(&dir), "gmail", None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("NUCLEUS_PERSONA_GMAIL"), "got: {msg}");
    }

    #[test]
    fn errors_when_persona_file_missing() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempdir();
        std::env::set_var("NUCLEUS_PERSONA_DISCORD", "ghost");
        let err = resolve_persona(&ident(&dir), "discord", None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("ghost") && msg.contains("personas"), "got: {msg}");
        std::env::remove_var("NUCLEUS_PERSONA_DISCORD");
    }

    #[test]
    fn context_override_wins_over_venue_default() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempdir();
        std::fs::write(dir.join("personas/base.md"), "base body").unwrap();
        std::fs::write(dir.join("personas/dm.md"), "dm body").unwrap();
        std::env::set_var("NUCLEUS_PERSONA_WHATSAPP", "base");
        std::env::set_var("NUCLEUS_PERSONA_WHATSAPP_DM", "dm");
        let p = resolve_persona(&ident(&dir), "whatsapp", Some("dm")).unwrap();
        assert_eq!(p.body.trim(), "dm body");
        assert_eq!(p.display_name, "dm");
        std::env::remove_var("NUCLEUS_PERSONA_WHATSAPP");
        std::env::remove_var("NUCLEUS_PERSONA_WHATSAPP_DM");
    }

    #[test]
    fn context_falls_back_to_venue_default() {
        let _g = ENV_LOCK.lock().unwrap();
        let dir = tempdir();
        std::fs::write(dir.join("personas/base.md"), "base body").unwrap();
        std::env::set_var("NUCLEUS_PERSONA_WHATSAPP", "base");
        std::env::remove_var("NUCLEUS_PERSONA_WHATSAPP_DM");
        let p = resolve_persona(&ident(&dir), "whatsapp", Some("dm")).unwrap();
        assert_eq!(p.body.trim(), "base body");
        std::env::remove_var("NUCLEUS_PERSONA_WHATSAPP");
    }

    #[test]
    fn frontmatter_splitter_handles_documents_without_frontmatter() {
        let (fm, rest) = split_frontmatter("just a body\n");
        assert!(fm.is_none());
        assert_eq!(rest, "just a body\n");
    }
}
