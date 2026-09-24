//! Typed settings.
//!
//! What belongs in `.env` vs `nucleus.toml`: see `docs/SECRETS.md`.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Settings {
    pub identity: Identity,
    pub public_urls: PublicUrls,
    pub claude: ClaudeConfig,
    pub discord: DiscordConfig,
    pub whatsapp: WhatsAppConfig,
    pub obsidian: ObsidianConfig,
    pub diary: DiaryConfig,
    pub distiller: DistillerConfig,
    pub skill_learner: SkillLearnerConfig,
    pub news: NewsConfig,
    pub gmail: GmailConfig,
    pub reminders: RemindersConfig,
    pub session_search: SessionSearchConfig,
    pub vault_search: VaultSearchConfig,
    pub vault_check: VaultCheckConfig,
    pub ports: PortsConfig,
}

/// Public-facing URLs for each tunnel-fronted service. All optional —
/// if a URL isn't set, that surface's tunnel health check is skipped and
/// any cross-link to it is hidden in the UI.
#[derive(Debug, Clone, Default)]
pub struct PublicUrls {
    pub nucleus: Option<String>,
    pub containers: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Identity {
    pub user_name: String,
    pub workspace_root: PathBuf,
    pub tier2_dir: PathBuf,
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

impl ObsidianConfig {
    /// `vault_path` with a leading `~/` expanded to `$HOME`.
    pub fn vault_dir(&self) -> std::path::PathBuf {
        expand_home(&self.vault_path)
    }
}

/// Expand a leading `~/` to `$HOME`. Config paths are operator-written and
/// routinely use `~`; every consumer would otherwise re-implement this.
pub fn expand_home(path: &str) -> std::path::PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => {
            let home = std::env::var("HOME").unwrap_or_default();
            std::path::PathBuf::from(home).join(rest)
        }
        None => std::path::PathBuf::from(path),
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DiaryConfig {
    pub root: String,
    pub retain_days: u32,
}

/// Distiller settings. Consolidated to a single daily pass (ADR-016); the
/// old hourly/weekly cron split is gone. All fields default so an operator's
/// pre-ADR-016 `nucleus.toml` (with `metabolism_cron` etc.) still loads —
/// the unknown keys are ignored and `cron` falls back to the daily default.
/// Note: the binary doesn't read these today (the real schedule lives in the
/// plist); they're documentary + reserved.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DistillerConfig {
    #[serde(default = "default_distiller_cron")]
    pub cron: String,
    #[serde(default)]
    pub model: Option<String>,
}

impl Default for DistillerConfig {
    fn default() -> Self {
        Self {
            cron: default_distiller_cron(),
            model: None,
        }
    }
}

fn default_distiller_cron() -> String {
    "0 4 * * *".to_string()
}

/// Skill-gap learner settings (ADR-017). All defaulted so an operator's
/// nucleus.toml without a `[skill_learner]` table still loads.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SkillLearnerConfig {
    /// On-the-fly: review a conversation after this many turns per chat.
    #[serde(default = "default_nudge_interval")]
    pub nudge_interval: u32,
    /// Periodic `learn` schedule (informational; real schedule in the plist).
    #[serde(default = "default_skill_learner_cron")]
    pub cron: String,
    /// Agent-created skills idle this long are marked stale.
    #[serde(default = "default_stale_days")]
    pub stale_after_days: u32,
    /// Agent-created skills idle this long are auto-archived (never deleted).
    #[serde(default = "default_archive_days")]
    pub archive_after_days: u32,
    /// Master switch for the on-the-fly arm (the periodic arm is gated by
    /// whether the launchd job is installed).
    #[serde(default = "default_true_bool")]
    pub enabled: bool,
}

impl Default for SkillLearnerConfig {
    fn default() -> Self {
        Self {
            nudge_interval: default_nudge_interval(),
            cron: default_skill_learner_cron(),
            stale_after_days: default_stale_days(),
            archive_after_days: default_archive_days(),
            enabled: true,
        }
    }
}

fn default_nudge_interval() -> u32 {
    12
}
fn default_skill_learner_cron() -> String {
    "30 4 * * *".to_string()
}
fn default_stale_days() -> u32 {
    30
}
fn default_archive_days() -> u32 {
    90
}
fn default_true_bool() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct NewsConfig {
    /// Informational — the real schedule is the plist's
    /// `StartCalendarInterval` (ADR-031: 09:00 and 19:00 local).
    pub fetch_cron: String,
    /// Vault-relative path of the prose profile of the reader. This note IS
    /// the ranking rubric (ADR-031) — the operator owns it, the fetcher only
    /// reads it, and a run without it fails rather than guessing.
    #[serde(default = "default_news_profile_note")]
    pub profile_note: String,
    /// Relevance floor for what reaches the widget. Not a count cap — a day
    /// with twenty items above the floor surfaces twenty.
    #[serde(default = "default_news_min_score")]
    pub min_score: f64,
    /// Directory the notch widget reads its feeds from. The fetcher writes
    /// `news.json` here and reads `news-votes.json` back out of it.
    #[serde(default = "default_news_widget_feed_dir")]
    pub widget_feed_dir: String,
    /// Case-insensitive regex matched against each fetched item's title and
    /// summary; a match drops the item before ranking. Empty disables it.
    #[serde(default = "default_news_exclude_title_regex")]
    pub exclude_title_regex: String,
}

impl NewsConfig {
    /// `widget_feed_dir` with a leading `~/` expanded to `$HOME`.
    pub fn widget_feed_path(&self) -> std::path::PathBuf {
        expand_home(&self.widget_feed_dir)
    }

    pub fn exclude_regex(&self) -> Option<regex::Regex> {
        let pat = self.exclude_title_regex.trim();
        if pat.is_empty() {
            return None;
        }
        regex::Regex::new(pat).ok()
    }
}

fn default_news_profile_note() -> String {
    "4-Areas/Nucleus/news-profile.md".to_string()
}

fn default_news_min_score() -> f64 {
    0.35
}

fn default_news_widget_feed_dir() -> String {
    "~/Library/Application Support/NotchWidget".to_string()
}

fn default_news_exclude_title_regex() -> String {
    String::new()
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

/// ADR-023 session-search maintenance knobs. `prune_apply` arms the junk
/// transcript deletion the distiller runs daily — false ships as the
/// default so a fresh install (and the first production week) only
/// reports what WOULD be deleted.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionSearchConfig {
    #[serde(default)]
    pub prune_apply: bool,
    #[serde(default = "default_prune_max_age_days")]
    pub prune_max_age_days: i64,
}

fn default_prune_max_age_days() -> i64 {
    14
}

impl Default for SessionSearchConfig {
    fn default() -> Self {
        Self { prune_apply: false, prune_max_age_days: default_prune_max_age_days() }
    }
}

/// ADR-035 vault search. `exclude` ADDS to the built-in exclusion floor in
/// `nucleus_core::vault::exclude` (dot folders, credential-like names, the
/// homelab credentials area); it can never remove an entry from that floor.
/// Globs are vault-relative and case-insensitive; a pattern without `/`
/// matches any single path component.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VaultSearchConfig {
    #[serde(default = "default_vault_exclude")]
    pub exclude: Vec<String>,
    /// Extra case-insensitive, multi-line regex for credential notes, added
    /// to the built-in detector in `nucleus_core::vault::exclude` (which
    /// cannot be turned off). A note whose text matches is never indexed,
    /// never returned, and never touched by `vault-check`. Empty = only the
    /// built-in detector.
    #[serde(default)]
    pub credential_content_regex: String,
}

impl Default for VaultSearchConfig {
    fn default() -> Self {
        Self { exclude: default_vault_exclude(), credential_content_regex: String::new() }
    }
}

/// `[vault_search]` as `<workspace_root>/nucleus.toml` states it now, read
/// on its own so a long-running process (the dashboard) can apply a changed
/// exclusion without a restart. A missing file or table means the defaults;
/// an unreadable file or invalid TOML is an error, so a caller fails closed
/// instead of falling back to weaker rules.
pub fn load_vault_search(workspace_root: &Path) -> Result<VaultSearchConfig> {
    #[derive(Deserialize)]
    struct Partial {
        #[serde(default)]
        vault_search: VaultSearchConfig,
    }
    let path = workspace_root.join("nucleus.toml");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(VaultSearchConfig::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let partial: Partial = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(partial.vault_search)
}

fn default_vault_exclude() -> Vec<String> {
    vec![
        "**/attachments/**".to_string(),
        "**/_attachments/**".to_string(),
        "**/assets/**".to_string(),
    ]
}

/// ADR-035 weekly vault check. All defaulted, so a nucleus.toml without a
/// `[vault_check]` table loads.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VaultCheckConfig {
    /// When `nucleus vault-check --scheduled` is due (5-field cron in
    /// `NUCLEUS_TZ`). The launchd job wakes hourly; this decides whether a
    /// wake runs the check. Default: Sunday 20:00.
    #[serde(default = "default_vault_check_cron")]
    pub cron: String,
    /// 0-Inbox notes older than this are reported.
    #[serde(default = "default_inbox_max_age_days")]
    pub inbox_max_age_days: i64,
    /// Frontmatter keys every note must carry (CLAUDE.md Rule 9.7).
    #[serde(default = "default_required_frontmatter")]
    pub required_frontmatter: Vec<String>,
    /// Allowed `source:` values. An entry ending in `*` is a prefix. A value
    /// joined with `+` (`a+b`) is valid when every part is.
    #[serde(default = "default_source_vocabulary")]
    pub source_vocabulary: Vec<String>,
    /// Notes never reported as orphans (hubs, journals, inbox, archive).
    #[serde(default = "default_orphan_exempt")]
    pub orphan_exempt: Vec<String>,
    /// Notes never reported for missing frontmatter or unknown source.
    #[serde(default = "default_frontmatter_exempt")]
    pub frontmatter_exempt: Vec<String>,
    /// Apply the safe fix on scheduled runs. Manual runs use `--apply`.
    #[serde(default)]
    pub scheduled_apply: bool,
    /// Enqueue the WhatsApp summary on scheduled runs.
    #[serde(default = "default_true_bool")]
    pub notify: bool,
}

impl Default for VaultCheckConfig {
    fn default() -> Self {
        Self {
            cron: default_vault_check_cron(),
            inbox_max_age_days: default_inbox_max_age_days(),
            required_frontmatter: default_required_frontmatter(),
            source_vocabulary: default_source_vocabulary(),
            orphan_exempt: default_orphan_exempt(),
            frontmatter_exempt: default_frontmatter_exempt(),
            scheduled_apply: false,
            notify: true,
        }
    }
}

fn default_vault_check_cron() -> String {
    "0 20 * * 0".to_string()
}
fn default_inbox_max_age_days() -> i64 {
    14
}
fn default_required_frontmatter() -> Vec<String> {
    vec!["created".to_string(), "source".to_string()]
}
/// The writers Nucleus itself ships plus the generic manual origins.
/// Operators add their own writers in nucleus.toml.
pub fn default_source_vocabulary() -> Vec<String> {
    [
        "whatsapp-braindump",
        "alfred-braindump",
        "chat-braindump",
        "distiller-contemplation",
        "obsidian-chat",
        "whatsapp-docstore",
        "whatsapp-chat",
        "nucleus-chat*",
        "nucleus-session",
        "claude-code*",
        "claude-session*",
        "voice-dictation",
        "deep-research",
        "research",
        "manual",
        "import",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}
fn default_orphan_exempt() -> Vec<String> {
    [
        "README.md",
        "index.md",
        "Home.md",
        "_*",
        "0-Inbox/**",
        "1-Main-Notes/**",
        "2-Daily-Notes/**",
        "7-Archives/**",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}
fn default_frontmatter_exempt() -> Vec<String> {
    ["README.md", "Home.md"].iter().map(|s| s.to_string()).collect()
}

fn default_reminder_channels() -> Vec<String> {
    vec!["discord-home".to_string()]
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PortsConfig {
    pub nucleus_dashboard: u16,
    /// Loopback port for the Bonsai image-generation FastAPI backend
    /// (ADR-019). Default 8093; the dashboard's /gallery surface proxies here.
    #[serde(default = "default_bonsai_port")]
    pub bonsai: u16,
}

fn default_bonsai_port() -> u16 {
    8093
}

// Intermediate struct for what we read from nucleus.toml.
#[derive(Debug, Deserialize)]
struct TomlConfig {
    claude: ClaudeConfig,
    discord: TomlDiscord,
    obsidian: ObsidianConfig,
    diary: DiaryConfig,
    #[serde(default)]
    distiller: DistillerConfig,
    #[serde(default)]
    skill_learner: SkillLearnerConfig,
    news: NewsConfig,
    gmail: GmailConfig,
    #[serde(default)]
    reminders: RemindersConfig,
    #[serde(default)]
    session_search: SessionSearchConfig,
    #[serde(default)]
    vault_search: VaultSearchConfig,
    #[serde(default)]
    vault_check: VaultCheckConfig,
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
            nucleus: env_optional("NUCLEUS_PUBLIC_URL"),
            containers: env_optional("NUCLEUS_CONTAINERS_PUBLIC_URL"),
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
            skill_learner: toml.skill_learner,
            news: toml.news,
            gmail,
            reminders: toml.reminders,
            session_search: toml.session_search,
            vault_search: toml.vault_search,
            vault_check: toml.vault_check,
            ports: toml.ports,
        })
    }

    /// The workspace root that every entry point resolves its paths against:
    /// the `memory/` DBs, logs, diaries, `agents.toml`, and the working
    /// directory of the sessions and processes it spawns.
    ///
    /// The value is `identity.workspace_root` (`NUCLEUS_WORKSPACE_ROOT` in
    /// `.env`), never the process's current directory: a `nucleus <command>`
    /// run from another directory must reach the operator's DBs, not create
    /// empty ones next to the caller. See [`resolve_workspace_root`].
    pub fn workspace_root(&self) -> Result<PathBuf> {
        let cwd = std::env::current_dir().ok();
        resolve_workspace_root(&self.identity.workspace_root, cwd.as_deref())
    }
}

/// Pure half of [`Settings::workspace_root`]. Rejects a relative root (it
/// would be resolved against the current directory, which is the failure
/// this function exists to prevent) and a root without a `memory/`
/// directory (a set-up workspace always has one: `memory/.gitkeep` is
/// tracked), so no DB is ever created in an unexpected place. Creates
/// nothing. `cwd` is only compared, for a debug log when it differs.
pub fn resolve_workspace_root(configured: &Path, cwd: Option<&Path>) -> Result<PathBuf> {
    if !configured.is_absolute() {
        bail!(
            "NUCLEUS_WORKSPACE_ROOT must be an absolute path, got {}",
            configured.display()
        );
    }
    let memory = configured.join("memory");
    if !memory.is_dir() {
        bail!(
            "{} does not exist or is not a directory; NUCLEUS_WORKSPACE_ROOT ({}) \
             must point at the Nucleus checkout",
            memory.display(),
            configured.display()
        );
    }
    let root = configured.canonicalize().unwrap_or_else(|_| configured.to_path_buf());
    if let Some(cwd) = cwd {
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        if cwd != root {
            tracing::debug!(
                cwd = %cwd.display(),
                workspace_root = %root.display(),
                "current directory differs from the workspace root; using the workspace root"
            );
        }
    }
    Ok(root)
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
mod workspace_root_tests {
    use super::*;

    fn temp_ws(name: &str, with_memory: bool) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "nucleus-wsroot-tests-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if with_memory {
            std::fs::create_dir_all(dir.join("memory")).unwrap();
        }
        dir.canonicalize().unwrap()
    }

    /// The configured root wins over a different current directory, so a
    /// call from elsewhere reaches the operator's DBs.
    #[test]
    fn configured_root_wins_over_cwd() {
        let ws = temp_ws("wins", true);
        let elsewhere = temp_ws("elsewhere", false);
        assert_eq!(resolve_workspace_root(&ws, Some(&elsewhere)).unwrap(), ws);
        assert_eq!(resolve_workspace_root(&ws, Some(&ws)).unwrap(), ws);
        assert_eq!(resolve_workspace_root(&ws, None).unwrap(), ws);
        assert!(!elsewhere.join("memory").exists());
    }

    /// A subdirectory of the workspace as cwd still resolves to the root.
    #[test]
    fn subdirectory_cwd_resolves_to_root() {
        let ws = temp_ws("subdir", true);
        let sub = ws.join("chores/reminders/src");
        std::fs::create_dir_all(&sub).unwrap();
        assert_eq!(resolve_workspace_root(&ws, Some(&sub)).unwrap(), ws);
        assert!(!sub.join("memory").exists());
    }

    /// A non-canonical configured path (with `..`) is returned canonical.
    #[test]
    fn configured_root_is_canonicalized() {
        let ws = temp_ws("canon", true);
        let dotted = ws.join("memory").join("..");
        assert_eq!(resolve_workspace_root(&dotted, None).unwrap(), ws);
    }

    /// A root without `memory/` is an error, and nothing is created there.
    #[test]
    fn missing_memory_dir_is_an_error_and_creates_nothing() {
        let ws = temp_ws("no-memory", false);
        let err = resolve_workspace_root(&ws, None).unwrap_err().to_string();
        assert!(err.contains("memory"), "{err}");
        assert!(!ws.join("memory").exists());
    }

    /// `memory` as a regular file does not count as a set-up workspace.
    #[test]
    fn memory_file_is_not_a_directory() {
        let ws = temp_ws("memory-file", false);
        std::fs::write(ws.join("memory"), "").unwrap();
        assert!(resolve_workspace_root(&ws, None).is_err());
    }

    #[test]
    fn relative_root_is_rejected() {
        let err = resolve_workspace_root(Path::new("nucleus"), None).unwrap_err().to_string();
        assert!(err.contains("absolute"), "{err}");
    }
}

#[cfg(test)]
mod vault_config_tests {
    use super::*;

    /// The documented example and the code defaults must not drift.
    #[test]
    fn example_vault_tables_match_defaults() {
        let example: toml::Value = toml::from_str(include_str!("../../nucleus.toml.example")).unwrap();
        let search: VaultSearchConfig = example["vault_search"].clone().try_into().unwrap();
        let check: VaultCheckConfig = example["vault_check"].clone().try_into().unwrap();
        let (ds, dc) = (VaultSearchConfig::default(), VaultCheckConfig::default());
        assert_eq!(search.exclude, ds.exclude);
        assert_eq!(search.credential_content_regex, ds.credential_content_regex);
        assert_eq!(check.cron, dc.cron);
        assert_eq!(check.inbox_max_age_days, dc.inbox_max_age_days);
        assert_eq!(check.required_frontmatter, dc.required_frontmatter);
        assert_eq!(check.source_vocabulary, dc.source_vocabulary);
        assert_eq!(check.orphan_exempt, dc.orphan_exempt);
        assert_eq!(check.frontmatter_exempt, dc.frontmatter_exempt);
        assert_eq!((check.scheduled_apply, check.notify), (dc.scheduled_apply, dc.notify));
    }

    #[test]
    fn missing_tables_use_defaults() {
        let search: VaultSearchConfig = toml::from_str("").unwrap();
        let check: VaultCheckConfig = toml::from_str("").unwrap();
        assert!(search.credential_content_regex.is_empty());
        assert_eq!(check.cron, "0 20 * * 0");
        assert!(!check.scheduled_apply);
    }
}

#[cfg(test)]
mod distiller_config_tests {
    use super::*;

    #[test]
    fn new_distiller_shape_loads() {
        let cfg: DistillerConfig =
            toml::from_str("cron = \"30 3 * * *\"\nmodel = \"claude-sonnet-4-6\"").unwrap();
        assert_eq!(cfg.cron, "30 3 * * *");
        assert_eq!(cfg.model.as_deref(), Some("claude-sonnet-4-6"));
    }

    #[test]
    fn empty_distiller_table_uses_defaults() {
        let cfg: DistillerConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.cron, "0 4 * * *");
    }
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
