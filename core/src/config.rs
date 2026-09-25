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
    pub usage: UsageConfig,
    pub vault_search: VaultSearchConfig,
    pub vault_check: VaultCheckConfig,
    pub tasks: TasksConfig,
    pub intake: IntakeConfig,
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

/// ADR-034 usage accounting. Every field is optional: the defaults read the
/// standard Claude Code and Codex transcript locations and use the built-in
/// price table (`nucleus_core::usage::pricing`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct UsageConfig {
    /// Claude Code transcript root. Tilde-expanded.
    #[serde(default = "default_claude_projects_dir")]
    pub claude_projects_dir: String,
    /// Codex session-log root. Tilde-expanded.
    #[serde(default = "default_codex_sessions_dir")]
    pub codex_sessions_dir: String,
    /// Path fragments that mark a git worktree nested inside its repository
    /// (`<repo>/.claude/worktrees/<name>`). A working directory that contains
    /// one is attributed to the part before the fragment, also after the
    /// worktree directory is deleted.
    #[serde(default = "default_worktree_markers")]
    pub worktree_markers: Vec<String>,
    /// Per-model price overrides and additions in USD per million tokens,
    /// keyed by model id or model-id prefix. Merged over the built-in table.
    #[serde(default)]
    pub prices: std::collections::BTreeMap<String, ModelPrice>,
}

/// Price of one model in USD per million tokens. `cache_write_5m` and
/// `cache_write_1h` are the two Anthropic cache-write durations; a vendor
/// with one cache-write price sets both to the same value.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct ModelPrice {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
    /// Rates for a request whose prompt exceeds a size, applied to the whole
    /// request (OpenAI's long-context pricing).
    #[serde(default)]
    pub long_context: Option<LongContextPrice>,
}

/// Whole-request rates above a prompt size, USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct LongContextPrice {
    /// The rates apply when the request's input tokens (uncached + cached +
    /// cache writes) are strictly more than this.
    pub above_input_tokens: i64,
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
}

fn default_claude_projects_dir() -> String {
    "~/.claude/projects".to_string()
}

fn default_codex_sessions_dir() -> String {
    "~/.codex/sessions".to_string()
}

fn default_worktree_markers() -> Vec<String> {
    vec!["/.claude/worktrees/".to_string(), "/.worktrees/".to_string()]
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            claude_projects_dir: default_claude_projects_dir(),
            codex_sessions_dir: default_codex_sessions_dir(),
            worktree_markers: default_worktree_markers(),
            prices: Default::default(),
        }
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
    /// Dashboard search: skip the index update when the last successful
    /// update started less than this many seconds ago and the vault's
    /// watermark (every note's path, size, mtime and inode, plus the
    /// exclusion rules) has not changed since.
    #[serde(default = "default_reindex_fresh_secs")]
    pub dashboard_reindex_fresh_secs: u64,
    /// Dashboard search: the longest a request waits for an index update
    /// before it answers 503. The update keeps running; later requests
    /// share it.
    #[serde(default = "default_reindex_wait_secs")]
    pub dashboard_reindex_wait_secs: u64,
}

impl Default for VaultSearchConfig {
    fn default() -> Self {
        Self {
            exclude: default_vault_exclude(),
            credential_content_regex: String::new(),
            dashboard_reindex_fresh_secs: default_reindex_fresh_secs(),
            dashboard_reindex_wait_secs: default_reindex_wait_secs(),
        }
    }
}

fn default_reindex_fresh_secs() -> u64 {
    30
}

fn default_reindex_wait_secs() -> u64 {
    20
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

/// Background task workers (ADR-033). Internal safety limits only — the
/// operator never sees them unless one is hit, and every hit is logged as a
/// task event.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TasksConfig {
    /// Workers allowed to run at once. A task started past this limit stays
    /// `queued` until a slot frees.
    #[serde(default = "default_tasks_max_concurrent")]
    pub max_concurrent: u32,
    /// A worker running longer than this is stopped and marked failed.
    #[serde(default = "default_tasks_max_runtime_hours")]
    pub max_runtime_hours: u32,
    /// tmux session that hosts the worker windows. Change it only to keep a
    /// second workspace (a test run) apart from the operator's workers.
    #[serde(default = "default_tasks_tmux_session")]
    pub tmux_session: String,
    /// Operator-facing status lines for finished tasks (`[tasks.texts]`).
    #[serde(default)]
    pub texts: TaskTexts,
}

/// The status line a finished task's result message starts with, per final
/// status. `{id}` is the short task id, `{title}` the task title. These go to
/// the task's origin (WhatsApp or Discord), so they live with the task
/// ledger, not with one venue.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TaskTexts {
    pub done: String,
    pub failed: String,
    pub cancelled: String,
    pub interrupted: String,
    /// The one note sent when a result's delivery is given up: its outcome
    /// is unknown (the send failed after the message may have left) or it
    /// failed too many times. `{reason}` is the recorded reason.
    pub delivery_failed: String,
}

impl Default for TaskTexts {
    fn default() -> Self {
        Self {
            done: "✅ Task {id} done — {title}".into(),
            failed: "⚠️ Task {id} failed — {title}".into(),
            cancelled: "⏹ Task {id} cancelled — {title}".into(),
            interrupted: "⚠️ Task {id} interrupted — {title}".into(),
            delivery_failed: "⚠️ Task {id} — {title}: the result message was not confirmed as delivered \
                              ({reason}). It is not sent again, so that it cannot arrive twice. The full \
                              result is in the task ledger (dashboard, Tasks page; `nucleus tasks status {id}`)."
                .into(),
        }
    }
}

fn default_tasks_tmux_session() -> String {
    crate::tasks::TASKS_TMUX_SESSION.to_string()
}

fn default_tasks_max_concurrent() -> u32 {
    6
}

fn default_tasks_max_runtime_hours() -> u32 {
    12
}

impl Default for TasksConfig {
    fn default() -> Self {
        Self {
            max_concurrent: default_tasks_max_concurrent(),
            max_runtime_hours: default_tasks_max_runtime_hours(),
            tmux_session: default_tasks_tmux_session(),
            texts: TaskTexts::default(),
        }
    }
}

/// Event intake and the issue pipeline (ADR-036). Off unless `enabled` and
/// at least one repo is configured. The repos name the operator's projects,
/// so they live in the untracked `nucleus.toml`; the committed example uses
/// placeholders.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IntakeConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Where the pipeline keeps its clones and per-item worktrees. Must be
    /// outside the Nucleus checkout. `~/` is expanded.
    #[serde(default = "default_intake_work_dir")]
    pub work_dir: String,
    /// The label that admits an issue into the pipeline. Only people with
    /// triage or write access can set labels on a GitHub issue, so the label
    /// is the proof that someone accepted the issue.
    #[serde(default = "default_intake_label")]
    pub label: String,
    /// An eval that says "simple" with a confidence below this value is
    /// treated as complex (it goes to refinement).
    #[serde(default = "default_intake_min_confidence")]
    pub min_confidence: f64,
    /// The repo's tests run by Nucleus after the implementation agent
    /// finished are stopped after this many minutes.
    #[serde(default = "default_intake_test_timeout_minutes")]
    pub test_timeout_minutes: u32,
    /// Author and committer of the one commit Nucleus publishes per item.
    /// The agent's own commits, authors and messages are never published.
    #[serde(default = "default_intake_commit_author_name")]
    pub commit_author_name: String,
    #[serde(default = "default_intake_commit_author_email")]
    pub commit_author_email: String,
    /// Limits checked on the agent's clone before any file is read into
    /// git: the number of paths (tracked and untracked, not ignored), each
    /// file's length (a sparse file counts by its length) and the total.
    #[serde(default = "default_intake_import_max_files")]
    pub import_max_files: usize,
    #[serde(default = "default_intake_import_max_file_bytes")]
    pub import_max_file_bytes: u64,
    #[serde(default = "default_intake_import_max_total_bytes")]
    pub import_max_total_bytes: u64,
    /// Largest `.gitignore` read (into a private copy) to decide which
    /// untracked files are ignored; counted toward the total.
    #[serde(default = "default_intake_import_max_ignore_bytes")]
    pub import_max_ignore_bytes: u64,
    /// Most directory entries the import walk reads (counted as read).
    #[serde(default = "default_intake_import_max_entries")]
    pub import_max_entries: usize,
    /// Largest diff the secret guard reads; a longer diff blocks the item
    /// (it is never cut and passed).
    #[serde(default = "default_intake_scan_max_bytes")]
    pub scan_max_bytes: usize,
    /// Hold an item when its issue text or a comment it uses has content
    /// that GitHub's page does not show (an HTML comment, invisible
    /// characters, collapsed or dropped Markdown): no agent runs until the
    /// operator releases or cancels it (ADR-036, "The hidden-content hold").
    #[serde(default = "default_true_bool")]
    pub hidden_content_hold: bool,
    #[serde(default)]
    pub repos: Vec<IntakeRepo>,
    #[serde(default)]
    pub github: IntakeGithubConfig,
    #[serde(default)]
    pub whatsapp: IntakeWhatsAppConfig,
    #[serde(default)]
    pub texts: IntakeTexts,
}

/// One repository the pipeline works on (`[[intake.repos]]`).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IntakeRepo {
    /// `owner/name` on GitHub.
    pub repo: String,
    /// The branch pull requests target. Default: the remote's HEAD branch.
    #[serde(default)]
    pub default_branch: Option<String>,
    /// Shell command that runs the repo's tests in a worktree
    /// (`sh -c`). Given to the implementation agent, and run again by
    /// Nucleus before the pull request. None: no test run.
    #[serde(default)]
    pub test_command: Option<String>,
    /// Keyword that links the pull request to the issue in the PR body
    /// (`Closes` closes the issue when the PR is merged; `Refs` only links).
    #[serde(default = "default_intake_issue_keyword")]
    pub pr_issue_keyword: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IntakeGithubConfig {
    /// The `gh` binary. launchd has no shell PATH (Rule 5); an absolute
    /// path or a PATH in the plist is needed there.
    #[serde(default = "default_intake_gh_bin")]
    pub gh_bin: String,
    /// Seconds between two polls of the same repo. The tick runs every
    /// minute; it polls only when this much time has passed.
    #[serde(default = "default_intake_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// How long a collaborator check is reused.
    #[serde(default = "default_intake_collaborator_cache_secs")]
    pub collaborator_cache_secs: u64,
    /// Upper bound on pages (100 issues each) read in one poll.
    #[serde(default = "default_intake_max_pages")]
    pub max_pages: u32,
    /// The URL Nucleus fetches from and pushes to; `{repo}` is replaced by
    /// `owner/name`. Nucleus never reads a remote URL from a clone.
    #[serde(default = "default_intake_remote_url")]
    pub remote_url: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IntakeWhatsAppConfig {
    /// Create a WhatsApp group (the bot and the operator only) for each item
    /// that reaches refinement. Off: every item thread runs in the DM.
    #[serde(default = "default_true_bool")]
    pub refinement_groups: bool,
    /// At most this many groups are created in any 24 hours. Automated group
    /// creation from a personal account can trigger WhatsApp's anti-spam
    /// checks; past the limit the item's thread runs in the DM.
    #[serde(default = "default_intake_max_groups_per_day")]
    pub max_groups_per_day: u32,
    /// A group that the bot has not created after this many minutes (bot
    /// offline, creation failed without an answer) is given up and the
    /// thread runs in the DM.
    #[serde(default = "default_intake_group_wait_minutes")]
    pub group_wait_minutes: u32,
}

/// Operator-facing texts of the pipeline (`[intake.texts]`). Placeholders:
/// `{n}` item number, `{title}`, `{ref}` (`owner/name#12`), `{url}`,
/// `{stage}`, `{version}`, `{error}`, `{pr_url}`, `{tests}`, `{comment}`,
/// `{summary}`, `{classification}`, `{failed_in}` (the stage a failed item
/// failed in), `{label}` (the gate label). `item_held` also has `{count}`
/// (the number of findings), `{kinds}` (the findings counted by kind) and
/// `{findings}` (the first findings, one per line); `item_released` has
/// `{via}` (where the operator released it). Operator commands start with `#{n}`: in the
/// DM the marker routes the message to the item; in the item's group it is
/// optional.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct IntakeTexts {
    pub refinement_opened: String,
    pub simple_started: String,
    pub approve_hint: String,
    pub plan_approved: String,
    pub pr_opened: String,
    pub comment_proposal: String,
    pub comment_posted: String,
    pub comment_skipped: String,
    pub item_failed: String,
    pub item_cancelled: String,
    pub item_closed: String,
    /// The item stopped because its source changed after the gate was
    /// satisfied (`{error}` says what changed; `{label}` is the gate label).
    pub item_stale: String,
    /// The secret guard stopped a publishing step (`{error}` lists the
    /// finding categories, never the matched text).
    pub item_blocked: String,
    /// The item is held: its issue text has content GitHub's page does not
    /// show (`{count}`, `{kinds}`, `{findings}`).
    pub item_held: String,
    /// The operator released a held item (`{via}`, `{stage}`).
    pub item_released: String,
    pub stage_note: String,
    pub no_plan: String,
    pub refinement_busy: String,
    pub unknown_item: String,
    /// The comment Nucleus proposes for the issue once the draft PR is open.
    pub issue_comment: String,
}

impl Default for IntakeTexts {
    fn default() -> Self {
        Self {
            refinement_opened: "🧭 Item #{n} — {title} ({ref}) needs a plan before implementation \
                                (eval: {classification}). {url}"
                .into(),
            simple_started: "🛠 Item #{n} — {title} ({ref}) was evaluated as simple; implementation \
                             started. {url}"
                .into(),
            approve_hint: "Reply `#{n} approve` to approve plan v{version}, or reply \
                           `#{n} <message>` to keep discussing (in the item's group the `#{n}` \
                           is optional)."
                .into(),
            plan_approved: "✅ Plan v{version} of item #{n} approved; implementation started.".into(),
            pr_opened: "📬 Draft PR for item #{n} — {title}: {pr_url}\nTests: {tests}".into(),
            comment_proposal: "Proposed comment on {ref}. Reply `#{n} approve comment` to post \
                               it or `#{n} skip comment` to post nothing; the dashboard can \
                               edit it first.\n\n{comment}"
                .into(),
            comment_posted: "💬 Comment posted on {ref}. Item #{n} is closed.".into(),
            comment_skipped: "Item #{n} is closed without a comment on {ref}.".into(),
            item_failed: "⚠️ Item #{n} — {title} failed during {failed_in}: {error}\nRetry from the \
                          dashboard (Intake page) or with `nucleus intake retry {n}`."
                .into(),
            item_cancelled: "⏹ Item #{n} — {title} cancelled.".into(),
            item_closed: "Item #{n} — {title} is closed: {error}".into(),
            item_stale: "⛔ Item #{n} — {title} stopped: {error}. Nothing more is done for it. To work \
                         on the issue as it is now, remove the `{label}` label and add it again; that \
                         starts a new item."
                .into(),
            item_blocked: "🛑 Item #{n} — {title} is blocked: {error}. Nothing was published. Fix the \
                           cause, then retry with `nucleus intake retry {n}` or on the dashboard, or \
                           cancel the item."
                .into(),
            item_held: "🔍 Item #{n} — {title} is held: the issue text has content that GitHub's page does \
                        not show ({kinds}). No agent runs until you decide.\n{findings}\nSee the dashboard \
                        (Intake page) for the full list. Reply `#{n} release` to continue with this content (the \
                        agent reads it as data), or `#{n} cancel`."
                .into(),
            item_released: "▶️ Item #{n} released via {via}; it continues in the {stage} stage. The hidden \
                            content reaches the agent as data, marked as released by you."
                .into(),
            stage_note: "Item #{n} is in the {stage} stage; messages reach an agent only during \
                         refinement. Your message is saved in the item's thread."
                .into(),
            no_plan: "Item #{n} has no plan to approve yet.".into(),
            refinement_busy: "The agent is still answering in item #{n}; approve after its reply \
                              arrives, so you approve the plan you read."
                .into(),
            unknown_item: "There is no open item #{n}.".into(),
            issue_comment: "A draft pull request for this issue is open: {pr_url}\n\n{summary}".into(),
        }
    }
}

fn default_intake_work_dir() -> String {
    "~/nucleus-work".into()
}
fn default_intake_label() -> String {
    "nucleus".into()
}
fn default_intake_min_confidence() -> f64 {
    0.7
}
fn default_intake_test_timeout_minutes() -> u32 {
    30
}
fn default_intake_commit_author_name() -> String {
    "Nucleus issue pipeline".into()
}
fn default_intake_commit_author_email() -> String {
    "nucleus-intake@localhost".into()
}
fn default_intake_import_max_files() -> usize {
    20_000
}
fn default_intake_import_max_file_bytes() -> u64 {
    10 * 1024 * 1024
}
fn default_intake_import_max_total_bytes() -> u64 {
    200 * 1024 * 1024
}
fn default_intake_import_max_ignore_bytes() -> u64 {
    1024 * 1024
}
fn default_intake_import_max_entries() -> usize {
    1_000_000
}
fn default_intake_scan_max_bytes() -> usize {
    16 * 1024 * 1024
}
fn default_intake_issue_keyword() -> String {
    "Closes".into()
}
fn default_intake_gh_bin() -> String {
    "gh".into()
}
fn default_intake_poll_interval_secs() -> u64 {
    300
}
fn default_intake_collaborator_cache_secs() -> u64 {
    3600
}
fn default_intake_max_pages() -> u32 {
    10
}
fn default_intake_remote_url() -> String {
    "https://github.com/{repo}.git".into()
}
fn default_intake_max_groups_per_day() -> u32 {
    3
}
fn default_intake_group_wait_minutes() -> u32 {
    15
}

impl Default for IntakeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            work_dir: default_intake_work_dir(),
            label: default_intake_label(),
            min_confidence: default_intake_min_confidence(),
            test_timeout_minutes: default_intake_test_timeout_minutes(),
            commit_author_name: default_intake_commit_author_name(),
            commit_author_email: default_intake_commit_author_email(),
            import_max_files: default_intake_import_max_files(),
            import_max_file_bytes: default_intake_import_max_file_bytes(),
            import_max_total_bytes: default_intake_import_max_total_bytes(),
            import_max_ignore_bytes: default_intake_import_max_ignore_bytes(),
            import_max_entries: default_intake_import_max_entries(),
            scan_max_bytes: default_intake_scan_max_bytes(),
            hidden_content_hold: true,
            repos: vec![],
            github: IntakeGithubConfig::default(),
            whatsapp: IntakeWhatsAppConfig::default(),
            texts: IntakeTexts::default(),
        }
    }
}

impl Default for IntakeGithubConfig {
    fn default() -> Self {
        Self {
            gh_bin: default_intake_gh_bin(),
            poll_interval_secs: default_intake_poll_interval_secs(),
            collaborator_cache_secs: default_intake_collaborator_cache_secs(),
            max_pages: default_intake_max_pages(),
            remote_url: default_intake_remote_url(),
        }
    }
}

impl Default for IntakeWhatsAppConfig {
    fn default() -> Self {
        Self {
            refinement_groups: true,
            max_groups_per_day: default_intake_max_groups_per_day(),
            group_wait_minutes: default_intake_group_wait_minutes(),
        }
    }
}

impl IntakeConfig {
    /// The configured repo `owner/name`, compared case-insensitively.
    pub fn repo(&self, name: &str) -> Option<&IntakeRepo> {
        self.repos.iter().find(|r| r.repo.eq_ignore_ascii_case(name.trim()))
    }

    /// `work_dir` with `~/` expanded.
    pub fn work_dir_path(&self) -> PathBuf {
        expand_home(&self.work_dir)
    }
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
    usage: UsageConfig,
    #[serde(default)]
    vault_search: VaultSearchConfig,
    #[serde(default)]
    vault_check: VaultCheckConfig,
    #[serde(default)]
    tasks: TasksConfig,
    #[serde(default)]
    intake: IntakeConfig,
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
            usage: toml.usage,
            vault_search: toml.vault_search,
            vault_check: toml.vault_check,
            tasks: toml.tasks,
            intake: toml.intake,
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
        assert_eq!(search.dashboard_reindex_fresh_secs, ds.dashboard_reindex_fresh_secs);
        assert_eq!(search.dashboard_reindex_wait_secs, ds.dashboard_reindex_wait_secs);
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
