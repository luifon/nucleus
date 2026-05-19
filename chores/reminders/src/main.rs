//! reminders — universal time-triggered notifications (ADR-006).
//!
//! Subcommands:
//!   add      schedule a reminder (--at xor --cron, --channels c1,c2,…)
//!   list     show reminders (active/pending by default)
//!   show     full detail for one reminder including channels + history
//!   cancel   terminate a reminder
//!   pause    temporarily disable; optional --until for auto-resume
//!   resume   reactivate a paused reminder
//!   history  query the per-fire audit log
//!   due      polling tick — run from launchd every minute

mod store;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Local, Utc};
use clap::{Parser, Subcommand};
use nucleus_core::{
    claude::PermissionMode,
    claude_session::{AskOptions, Session, SpawnOptions},
    config::{self, Settings},
    diary, discord_sdk,
};
use std::path::Path;
use std::time::Duration;

const AGENT_NAME: &str = "reminders";
const DB_PATH: &str = "memory/reminders.db";
const WHATSAPP_DB_PATH: &str = "memory/whatsapp.db";

#[derive(Parser)]
#[command(name = "reminders", about = "Nucleus scheduled reminders")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Schedule a reminder. Choose ONE of --at (one-shot) or --cron (recurring),
    /// and ONE of --body (post text) or --system-prompt (spawn a Claude session
    /// at fire time and orchestrate skills — ADR-008).
    Add {
        /// One-shot fire time. ISO-8601; offset optional (no offset = local TZ).
        #[arg(long, conflicts_with = "cron")]
        at: Option<String>,
        /// Standard 5-field cron expression, evaluated in NUCLEUS_TZ.
        #[arg(long, conflicts_with = "at")]
        cron: Option<String>,
        /// Text body posted at fire time. Mutually exclusive with --system-prompt.
        #[arg(long, conflicts_with = "system_prompt")]
        body: Option<String>,
        /// Instruction sent at fire time to a freshly-spawned one-shot Claude
        /// session. The session has all skills auto-loaded (Claude Code native);
        /// orchestrate them by referencing a skill or describing the task.
        /// Mutually exclusive with --body.
        #[arg(long = "system-prompt", conflicts_with = "body")]
        system_prompt: Option<String>,
        /// Comma-separated channels. For --body: delivery targets (default:
        /// discord-home). For --system-prompt: outer-error alert destinations
        /// (default: [reminders].default_channels in nucleus.toml, falling
        /// back to discord-home if unset).
        #[arg(long, value_delimiter = ',')]
        channels: Vec<String>,
    },
    /// List reminders (active/pending by default).
    List {
        #[arg(long)]
        include_fired: bool,
        #[arg(long)]
        include_cancelled: bool,
    },
    /// Full detail for one reminder, including channels + recent fires.
    Show {
        #[arg(value_name = "ID")]
        id: i64,
    },
    /// Cancel a reminder.
    Cancel {
        #[arg(value_name = "ID")]
        id: i64,
    },
    /// Pause a reminder. With --until, auto-resumes at that time.
    Pause {
        #[arg(value_name = "ID")]
        id: i64,
        #[arg(long)]
        until: Option<String>,
    },
    /// Resume a paused reminder.
    Resume {
        #[arg(value_name = "ID")]
        id: i64,
    },
    /// Query the fire history.
    History {
        #[arg(long)]
        days: Option<i64>,
        #[arg(long)]
        channel: Option<String>,
        #[arg(long)]
        reminder: Option<i64>,
    },
    /// Polling tick — find due reminders and deliver them.
    Due,
}

#[tokio::main]
async fn main() -> Result<()> {
    nucleus_core::init_tracing();
    let settings = Settings::load().context("loading settings")?;
    let workspace_root = std::env::current_dir()?;

    let cli = Cli::parse();
    match cli.command {
        Cmd::Add { at, cron, body, system_prompt, channels } => {
            add(
                &settings,
                &workspace_root,
                at.as_deref(),
                cron.as_deref(),
                body.as_deref(),
                system_prompt.as_deref(),
                channels,
            )
            .await
        }
        Cmd::List { include_fired, include_cancelled } => {
            list(&workspace_root, include_fired, include_cancelled).await
        }
        Cmd::Show { id } => show(&workspace_root, id).await,
        Cmd::Cancel { id } => cancel(&workspace_root, id).await,
        Cmd::Pause { id, until } => pause(&workspace_root, id, until.as_deref()).await,
        Cmd::Resume { id } => resume(&workspace_root, id).await,
        Cmd::History { days, channel, reminder } => {
            history(&workspace_root, days, channel.as_deref(), reminder).await
        }
        Cmd::Due => due(&settings, &workspace_root).await,
    }
}

async fn open_pool(workspace_root: &Path) -> Result<sqlx::SqlitePool> {
    let pool = store::open(&workspace_root.join(DB_PATH)).await?;
    // Idempotent: skips when the row is already there (or has been
    // cancelled — cancellation is sticky).
    store::seed_default_reminders(&pool).await?;
    Ok(pool)
}

async fn add(
    settings: &Settings,
    workspace_root: &Path,
    at: Option<&str>,
    cron: Option<&str>,
    body: Option<&str>,
    system_prompt: Option<&str>,
    channels: Vec<String>,
) -> Result<()> {
    // Resolve the body/system_prompt XOR. clap's `conflicts_with` already
    // rejects the (Some,Some) case at parse time; we still need the
    // (None,None) guard because we dropped the body default.
    let (body_stored, system_prompt_stored) = match (body, system_prompt) {
        (Some(b), None) => {
            if b.trim().is_empty() {
                bail!("--body cannot be empty");
            }
            (b.to_string(), None)
        }
        (None, Some(sp)) => {
            if sp.trim().is_empty() {
                bail!("--system-prompt cannot be empty");
            }
            // body NOT NULL in the schema; store empty for system-prompt
            // reminders. The system_prompt column is the source of truth.
            (String::new(), Some(sp.to_string()))
        }
        (Some(_), Some(_)) => unreachable!("clap conflicts_with prevents this"),
        (None, None) => bail!("must provide --body (text post) or --system-prompt (skill fire)"),
    };

    // Channel resolution. For body-based, default to discord-home. For
    // system-prompt-based, fall back to settings.reminders.default_channels,
    // and if that is empty too, still require discord-home so outer-error
    // alerts have somewhere to land (per Slice-3 channel semantics).
    let channels = if !channels.is_empty() {
        channels
    } else if system_prompt_stored.is_some() && !settings.reminders.default_channels.is_empty() {
        settings.reminders.default_channels.clone()
    } else {
        vec![store::CHANNEL_DISCORD_HOME.to_string()]
    };

    let pool = open_pool(workspace_root).await?;

    let (cron_expr, next_fire_at, one_shot) = match (at, cron) {
        (Some(_), Some(_)) => bail!("--at and --cron are mutually exclusive"),
        (None, None) => bail!("must provide --at (one-shot) or --cron (recurring)"),
        (Some(at_str), None) => {
            let at_local = store::parse_at(at_str)?;
            let (c, when_utc) = store::one_shot_cron(at_local);
            (c, when_utc, true)
        }
        (None, Some(c)) => {
            let tz = store::nucleus_tz();
            let next = store::next_match_utc(c, Utc::now(), tz)
                .map_err(|e| anyhow!("--cron rejected: {e}"))?;
            (c.to_string(), next, false)
        }
    };

    let id = store::insert_with_channels(
        &pool,
        &body_stored,
        &cron_expr,
        one_shot,
        next_fire_at,
        &channels,
        "user",
        system_prompt_stored.as_deref(),
    )
    .await?;
    let local = next_fire_at.with_timezone(&Local);
    let kind = if system_prompt_stored.is_some() { "skill-fire" } else { "body" };
    tracing::info!(
        id = id,
        kind = kind,
        next_fire_local = %local.format("%Y-%m-%d %H:%M %Z"),
        cron = %cron_expr,
        one_shot = one_shot,
        channels = ?channels,
        payload_len = system_prompt_stored.as_deref().map(str::len).unwrap_or(body_stored.len()),
        "reminder scheduled"
    );
    // Print id on stdout so callers can capture it.
    println!("{}", id);
    Ok(())
}

async fn list(
    workspace_root: &Path,
    include_fired: bool,
    include_cancelled: bool,
) -> Result<()> {
    let pool = open_pool(workspace_root).await?;
    let rows = store::list_all(&pool, include_fired, include_cancelled).await?;
    if rows.is_empty() {
        println!("(no reminders)");
        return Ok(());
    }
    for r in rows {
        let next = r
            .next_fire_at
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "—".to_string());
        let kind = if r.one_shot { "once" } else { "cron" };
        let extra = match r.status.as_str() {
            "paused" => match r.paused_until.as_deref() {
                Some(u) => format!(" [paused until {}]", u),
                None => " [paused]".to_string(),
            },
            "fired" => " [fired]".to_string(),
            "cancelled" => " [cancelled]".to_string(),
            _ => String::new(),
        };
        let channels = store::channels_for(&pool, r.id).await?;
        let ch: Vec<&str> = channels.iter().map(|c| c.channel.as_str()).collect();
        // 🪄 prefix marks system-prompt (skill-fire) reminders so it's
        // visually obvious which fires will spawn a Claude session.
        let payload_display = match r.system_prompt.as_deref() {
            Some(sp) => format!("🪄 {}", truncate(sp, 60)),
            None => r.body.clone(),
        };
        println!(
            "#{:<4} {}  [{}] {:>5}  ({})  {}{}",
            r.id,
            next,
            ch.join(","),
            kind,
            r.cron,
            payload_display,
            extra
        );
    }
    Ok(())
}

async fn show(workspace_root: &Path, id: i64) -> Result<()> {
    let pool = open_pool(workspace_root).await?;
    let r = store::get(&pool, id)
        .await?
        .ok_or_else(|| anyhow!("#{} not found", id))?;
    let local_next = r
        .next_fire_at
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Local).format("%Y-%m-%d %H:%M %Z").to_string())
        .unwrap_or_else(|| "—".into());

    println!("#{}  status={}  one_shot={}", r.id, r.status, r.one_shot);
    match r.system_prompt.as_deref() {
        Some(sp) => println!("  🪄 system_prompt: {}", sp),
        None => println!("  body: {}", r.body),
    }
    println!("  cron: {}", r.cron);
    println!("  next_fire: {}  (utc: {})",
        local_next,
        r.next_fire_at.as_deref().unwrap_or("—"));
    if let Some(l) = r.last_fired_at.as_deref() {
        println!("  last_fired: {}", l);
    }
    if let Some(p) = r.paused_until.as_deref() {
        println!("  paused_until: {}", p);
    }
    println!("  created: {} by {}", r.created_at, r.created_by);
    println!("  channels:");
    let chs = store::channels_for(&pool, r.id).await?;
    if chs.is_empty() {
        println!("    (none)");
    }
    for c in chs {
        println!(
            "    - {:12}  status={:8}  attempts={}{}",
            c.channel,
            c.status,
            c.attempts,
            c.last_error
                .as_deref()
                .map(|e| format!("  err={e}"))
                .unwrap_or_default()
        );
    }
    println!("  recent fires:");
    let fires = store::fire_history(&pool, Some(30), None, Some(r.id)).await?;
    if fires.is_empty() {
        println!("    (none)");
    }
    for f in fires.iter().take(10) {
        let outcome = if f.success { "ok " } else { "ERR" };
        let extra = if f.success {
            f.msg_id.clone().unwrap_or_default()
        } else {
            f.error.clone().unwrap_or_default()
        };
        println!(
            "    {}  {}  {:12}  {}  {}",
            f.fired_at, outcome, f.channel, f.id, extra
        );
    }
    Ok(())
}

async fn cancel(workspace_root: &Path, id: i64) -> Result<()> {
    let pool = open_pool(workspace_root).await?;
    let cancelled = store::cancel(&pool, id).await?;
    if cancelled {
        tracing::info!(id = id, "cancelled reminder");
        println!("cancelled #{}", id);
    } else {
        bail!("#{} not found (or already fired/cancelled)", id);
    }
    Ok(())
}

async fn pause(workspace_root: &Path, id: i64, until: Option<&str>) -> Result<()> {
    let pool = open_pool(workspace_root).await?;
    let until_utc = match until {
        Some(u) => Some(store::parse_at(u)?.with_timezone(&Utc)),
        None => None,
    };
    let ok = store::pause(&pool, id, until_utc).await?;
    if !ok {
        bail!("#{} not found (or not active/pending)", id);
    }
    match until_utc {
        Some(u) => println!(
            "paused #{} (auto-resumes {})",
            id,
            u.with_timezone(&Local).format("%Y-%m-%d %H:%M %Z")
        ),
        None => println!("paused #{} (indefinitely)", id),
    }
    Ok(())
}

async fn resume(workspace_root: &Path, id: i64) -> Result<()> {
    let pool = open_pool(workspace_root).await?;
    let ok = store::resume(&pool, id).await?;
    if !ok {
        bail!("#{} not found or not paused", id);
    }
    println!("resumed #{}", id);
    Ok(())
}

async fn history(
    workspace_root: &Path,
    days: Option<i64>,
    channel: Option<&str>,
    reminder: Option<i64>,
) -> Result<()> {
    let pool = open_pool(workspace_root).await?;
    let rows = store::fire_history(&pool, days, channel, reminder).await?;
    if rows.is_empty() {
        println!("(no fires recorded)");
        return Ok(());
    }
    for f in rows {
        let local = DateTime::parse_from_rfc3339(&f.fired_at)
            .map(|d| d.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or(f.fired_at.clone());
        let outcome = if f.success { "ok " } else { "ERR" };
        let extra = if f.success {
            f.msg_id.unwrap_or_default()
        } else {
            f.error.unwrap_or_default()
        };
        println!(
            "{}  #{:<4} {}  {:12}  {}",
            local, f.reminder_id, outcome, f.channel, extra
        );
    }
    Ok(())
}

async fn due(settings: &Settings, workspace_root: &Path) -> Result<()> {
    // Skill-fire reminders (ADR-008) can run for minutes; the launchd
    // plist ticks every 60s. Without a lock, a second tick would pick
    // up the same in-flight reminder (its channel rows are still
    // 'pending') and spawn a duplicate session. Hold a file lock for
    // the whole tick to serialize.
    let lock_path = workspace_root.join("memory/reminders-tick.lock");
    let _lock = match TickLock::try_acquire(lock_path)? {
        Some(l) => l,
        None => {
            tracing::info!("another reminders-tick is in progress; skipping this tick");
            return Ok(());
        }
    };

    let pool = open_pool(workspace_root).await?;
    let now = Utc::now();

    let resumed = store::auto_resume_paused(&pool, now).await?;
    if resumed > 0 {
        tracing::info!(count = resumed, "auto-resumed paused reminders");
    }

    let work = store::pending_due_with_channels(&pool, now).await?;
    if work.is_empty() {
        return Ok(());
    }
    tracing::info!(count = work.len(), "delivering due reminders");

    let mention = settings
        .discord
        .allowed_user_ids
        .first()
        .map(|id| format!("<@{}> ", id))
        .unwrap_or_default();

    for (reminder, channels) in work {
        let fired_at = Utc::now();
        if reminder.system_prompt.is_some() {
            // Skill-fire: spawn one session, then forward its reply to
            // each channel as the actual user-visible output. The
            // session has no built-in posting tool, so the reminders
            // worker is the bridge (see persona.md for the contract).
            match deliver_skill_fire(settings, workspace_root, &reminder, &channels).await {
                Ok(SkillFireResult { msg_id, reply }) => {
                    tracing::info!(
                        id = reminder.id,
                        msg = %msg_id,
                        reply_chars = reply.chars().count(),
                        "skill-fire ok"
                    );
                    let reply_capped = truncate(&reply, SKILL_REPLY_CAP_CHARS);
                    let forward_reminder = store::Reminder {
                        body: reply_capped,
                        ..reminder.clone()
                    };
                    for ch in &channels {
                        // Best-effort forward; if a channel rejects we
                        // still mark the channel sent (the fire itself
                        // succeeded — channel-level forward issues are
                        // a separate failure mode and we log them).
                        let forward = deliver(
                            settings,
                            workspace_root,
                            &mention,
                            &forward_reminder,
                            &ch.channel,
                        )
                        .await;
                        let recorded_msg_id = match forward {
                            Ok(fw_msg) => format!("{}|fwd:{}", msg_id, fw_msg),
                            Err(e) => {
                                tracing::warn!(
                                    id = reminder.id,
                                    channel = %ch.channel,
                                    err = %e,
                                    "skill-fire reply forward failed"
                                );
                                format!("{}|fwd-err:{}", msg_id, truncate(&e.to_string(), 80))
                            }
                        };
                        store::record_channel_fire(
                            &pool,
                            reminder.id,
                            &ch.channel,
                            fired_at,
                            Ok(&recorded_msg_id),
                        )
                        .await?;
                    }
                    let _ = diary::record_observation(
                        workspace_root,
                        AGENT_NAME,
                        "fired",
                        &format!(
                            "reminder #{} skill-fire → {}",
                            reminder.id,
                            channels
                                .iter()
                                .map(|c| c.channel.as_str())
                                .collect::<Vec<_>>()
                                .join(",")
                        ),
                        diary::Tag::Observation,
                    );
                }
                Err(e) => {
                    let err = e.to_string();
                    tracing::warn!(
                        id = reminder.id,
                        err = %err,
                        "skill-fire failed; alerting channels"
                    );
                    // Outer-error alert (ADR-008 failure handling layer 2):
                    // post a heads-up to each channel best-effort, then
                    // record the channel-fire as Err so per-channel retry
                    // budget governs whether we re-spawn next tick.
                    let alert_body = format!(
                        "⚠️ Reminder #{} fire failed: {}",
                        reminder.id,
                        truncate(&err, 200)
                    );
                    let alert_reminder = store::Reminder {
                        body: alert_body,
                        ..reminder.clone()
                    };
                    for ch in &channels {
                        let _ = deliver(
                            settings,
                            workspace_root,
                            &mention,
                            &alert_reminder,
                            &ch.channel,
                        )
                        .await;
                        store::record_channel_fire(
                            &pool,
                            reminder.id,
                            &ch.channel,
                            fired_at,
                            Err(&err),
                        )
                        .await?;
                    }
                }
            }
        } else {
            // Body-based: deliver per-channel (unchanged from ADR-006).
            for ch in channels {
                match deliver(settings, workspace_root, &mention, &reminder, &ch.channel).await {
                    Ok(msg_id) => {
                        store::record_channel_fire(
                            &pool,
                            reminder.id,
                            &ch.channel,
                            fired_at,
                            Ok(&msg_id),
                        )
                        .await?;
                        tracing::info!(
                            id = reminder.id,
                            channel = %ch.channel,
                            msg = %msg_id,
                            "fired"
                        );
                        let _ = diary::record_observation(
                            workspace_root,
                            AGENT_NAME,
                            "fired",
                            &format!(
                                "reminder #{} ({}c) → {}",
                                reminder.id,
                                reminder.body.len(),
                                ch.channel
                            ),
                            diary::Tag::Observation,
                        );
                    }
                    Err(e) => {
                        let err = e.to_string();
                        store::record_channel_fire(
                            &pool,
                            reminder.id,
                            &ch.channel,
                            fired_at,
                            Err(&err),
                        )
                        .await?;
                        tracing::warn!(
                            id = reminder.id,
                            channel = %ch.channel,
                            err = %err,
                            "deliver failed; per-channel retry will pick it up"
                        );
                    }
                }
            }
        }
        // Either advances the schedule (all channels terminal) or
        // leaves next_fire_at alone for the next tick to retry the
        // still-pending ones.
        if let Err(e) = store::advance_after_fire(&pool, reminder.id).await {
            tracing::warn!(id = reminder.id, err = %e, "advance_after_fire failed");
        }
    }
    Ok(())
}

/// Spawn a one-shot Claude session, send the reminder's stored
/// `system_prompt` as the first `ask()` payload, and return a synthetic
/// `msg_id` (`skill-fire:<session-id>`). Mirrors the calendar fire
/// pattern (`deliver_calendar`) but stays at the reminders layer — no
/// per-channel iteration; one session per fire.
async fn deliver_skill_fire(
    settings: &Settings,
    workspace_root: &Path,
    r: &store::Reminder,
    channels: &[store::ChannelRow],
) -> Result<SkillFireResult> {
    let system_prompt = r
        .system_prompt
        .as_deref()
        .ok_or_else(|| anyhow!("reminder #{} has no system_prompt", r.id))?;

    let persona_path = workspace_root.join("chores/reminders/persona.md");
    let persona = tokio::fs::read_to_string(&persona_path)
        .await
        .with_context(|| format!("reading {}", persona_path.display()))?;
    let persona = config::substitute(&persona, &settings.identity);

    let routing: Vec<&str> = channels.iter().map(|c| c.channel.as_str()).collect();
    let local_now = Local::now().format("%Y-%m-%d %H:%M %Z");
    let ask_payload = format!(
        "[reminder #{} fire — {}]\n\
         Default output routing (if the instruction below doesn't specify\n\
         and your task needs to post results somewhere): {}\n\n\
         {}",
        r.id,
        local_now,
        routing.join(", "),
        system_prompt,
    );

    let mut session = Session::spawn(SpawnOptions {
        workspace_root: workspace_root.to_path_buf(),
        append_system_prompt: Some(persona),
        permission_mode: Some(PermissionMode::Auto),
        disallowed_tools: settings.claude.disallowed_tools.clone(),
        // No allowed_tools pre-approval at this layer. Each skill's
        // own `allowed-tools` frontmatter (per Claude Code skills
        // docs) handles tool gating when invoked.
        tmux_session: "nucleus-reminders-fire".into(),
        window_name: Some(format!("fire-{}", r.id)),
        ready_timeout: Duration::from_secs(20),
        ..SpawnOptions::default()
    })
    .await
    .with_context(|| format!("spawning fire session for reminder #{}", r.id))?;

    // Generous timeout — Playwright-driven skills can take a few minutes.
    // Bounded by the per-tick file lock so we don't overlap.
    let raw = session
        .ask(
            &ask_payload,
            AskOptions {
                max_wait: Duration::from_secs(300),
                quiescent_window: Duration::from_secs(5),
            },
        )
        .await;
    let session_id = session.session_id.clone();
    let _ = session.close().await;
    let reply = raw.with_context(|| format!("ask() for reminder #{}", r.id))?;

    if reply.trim().is_empty() {
        bail!("session returned an empty reply (treated as failure)");
    }

    let _ = diary::record_observation(
        workspace_root,
        AGENT_NAME,
        "skill-fire",
        &format!("reminder #{}: {}", r.id, truncate(&reply, 200)),
        diary::Tag::Observation,
    );

    Ok(SkillFireResult {
        msg_id: format!("skill-fire:{}", session_id),
        reply,
    })
}

/// Discord's hard limit is 2000; cap below that to leave room for the
/// "🔔 **Reminder:** " prefix added in `deliver()`.
const SKILL_REPLY_CAP_CHARS: usize = 1800;

/// Outcome of a successful skill-fire spawn. The msg_id is what gets
/// written into reminder_fires; the reply is what the worker forwards
/// to the channels as the actual user-visible output.
struct SkillFireResult {
    msg_id: String,
    reply: String,
}

/// Best-effort serialization of `reminders due` ticks via a lockfile
/// in `memory/`. Skill-fire reminders can take minutes; the launchd
/// plist ticks every 60s, so without serialization a long fire would
/// be re-picked-up by the next tick (its channel rows are still
/// 'pending') and a duplicate session would spawn.
///
/// Stale-lock recovery: if the lockfile is older than 10 minutes we
/// assume the prior process crashed (SIGKILL leaves the file behind
/// because Drop didn't run) and reclaim it.
struct TickLock {
    path: std::path::PathBuf,
}

impl TickLock {
    fn try_acquire(path: std::path::PathBuf) -> Result<Option<Self>> {
        use std::fs::{OpenOptions, metadata};
        use std::io::ErrorKind;
        use std::time::SystemTime;

        if let Ok(meta) = metadata(&path) {
            let stale = meta
                .modified()
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .map(|d| d > Duration::from_secs(10 * 60))
                .unwrap_or(false);
            if stale {
                tracing::warn!(
                    path = %path.display(),
                    "stale reminders-tick lockfile (>10min); reclaiming"
                );
                let _ = std::fs::remove_file(&path);
            }
        }

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => Ok(Some(TickLock { path })),
            Err(e) if e.kind() == ErrorKind::AlreadyExists => Ok(None),
            Err(e) => Err(anyhow!(
                "opening tick lockfile {}: {e}",
                path.display()
            )),
        }
    }
}

impl Drop for TickLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

async fn deliver(
    settings: &Settings,
    workspace_root: &Path,
    mention: &str,
    r: &store::Reminder,
    channel: &str,
) -> Result<String> {
    match channel {
        store::CHANNEL_DISCORD_HOME => {
            if settings.discord.home_channel_id.is_empty() {
                bail!("DISCORD_HOME_CHANNEL_ID not set");
            }
            let body = format!("{}🔔 **Reminder:** {}", mention, r.body);
            discord_sdk::send_announcement(&settings.discord.home_channel_id, &body).await
        }
        store::CHANNEL_WHATSAPP_GROUP | store::CHANNEL_BRAINDUMP | store::CHANNEL_WHATSAPP_DM => {
            let env_var = match channel {
                store::CHANNEL_WHATSAPP_GROUP => "WHATSAPP_ALLOWED_GROUP_NAMES",
                store::CHANNEL_BRAINDUMP => "WHATSAPP_BRAINDUMP_GROUP_NAMES",
                store::CHANNEL_WHATSAPP_DM => "WHATSAPP_ALLOWED_DM_JIDS",
                _ => unreachable!(),
            };
            let target = first_csv_entry(env_var).ok_or_else(|| {
                anyhow!(
                    "channel {:?} requires {env_var} to be set with at least one entry",
                    channel
                )
            })?;
            let body = format!("🔔 *Reminder:* {}", r.body);
            let pool = store::open_whatsapp_db(&workspace_root.join(WHATSAPP_DB_PATH)).await?;
            let queue_id = store::enqueue_whatsapp(&pool, &target, &body, "reminders").await?;
            Ok(format!("whatsapp-queue#{}", queue_id))
        }
        store::CHANNEL_CALENDAR => deliver_calendar(settings, workspace_root, r).await,
        other => bail!("unknown channel {:?}", other),
    }
}

/// Schedule a calendar event by spawning a one-shot JARVIS session and
/// asking it to call the Google Calendar MCP. Per ADR-007, the calendar
/// lives on the configured trash account but the personal email is
/// added as attendee so the invite reaches the user's main calendar.
async fn deliver_calendar(
    settings: &Settings,
    workspace_root: &Path,
    r: &store::Reminder,
) -> Result<String> {
    if settings.gmail.personal_email.is_empty() {
        bail!("NUCLEUS_PERSONAL_EMAIL not set — calendar channel needs an attendee");
    }
    let next_fire = r
        .next_fire_at
        .as_deref()
        .ok_or_else(|| anyhow!("reminder #{} has no next_fire_at", r.id))?;
    let start_utc = DateTime::parse_from_rfc3339(next_fire)
        .with_context(|| format!("parsing next_fire_at {:?}", next_fire))?
        .with_timezone(&Utc);
    let duration = chrono::Duration::minutes(
        settings.gmail.calendar_default_duration_min.max(1) as i64,
    );
    let end_utc = start_utc + duration;

    // ADR-009: persona resolved from `personas/<slug>.md` via env var, not
    // from the deleted `messaging/gmail/persona.md` file. `${GMAIL_ACCOUNT}`
    // substitution happens after resolution, same as in metabolize.rs.
    let persona = config::resolve_persona(&settings.identity, "gmail", None)
        .context("resolving Gmail persona for calendar reminder (ADR-009)")?;
    let persona = config::substitute_gmail(&persona.body, &settings.gmail);

    let prompt = format!(
        r#"Schedule a calendar event using the `mcp__claude_ai_Google_Calendar__create_event` tool.

summary:    {summary}
start:      {start} (RFC3339 UTC)
end:        {end} (RFC3339 UTC)
attendees:  ["{attendee}"]
send_updates: "all"

After the tool succeeds, reply with ONLY the event id on its own line — no prose, no markdown."#,
        summary = r.body.replace('\n', " "),
        start = start_utc.to_rfc3339(),
        end = end_utc.to_rfc3339(),
        attendee = settings.gmail.personal_email,
    );

    // One-shot session — no SessionPool keying, the calendar channel doesn't
    // need conversational continuity.
    let mut session = Session::spawn(SpawnOptions {
        workspace_root: workspace_root.to_path_buf(),
        append_system_prompt: Some(persona),
        permission_mode: Some(PermissionMode::Auto),
        disallowed_tools: settings.claude.disallowed_tools.clone(),
        // Pre-approve the create_event MCP call. Without this, the
        // auto-mode classifier blocks invites sent to "external"
        // addresses — but our entire design point is to send invites
        // to NUCLEUS_PERSONAL_EMAIL. The persona already constrains
        // who may be addressed; the classifier guard is redundant
        // here and just makes calendar deliveries non-functional.
        allowed_tools: vec![
            "mcp__claude_ai_Google_Calendar__create_event".into(),
        ],
        tmux_session: "nucleus-jarvis".into(),
        window_name: Some(format!("cal-{}", r.id)),
        ready_timeout: Duration::from_secs(20),
        ..SpawnOptions::default()
    })
    .await
    .context("spawning JARVIS session for calendar event")?;

    let raw = session
        .ask(
            &prompt,
            AskOptions {
                max_wait: Duration::from_secs(60),
                quiescent_window: Duration::from_secs(3),
            },
        )
        .await;
    let _ = session.close().await;
    let raw = raw.context("JARVIS calendar create_event")?;

    // Validate the last non-empty line looks like a Google Calendar
    // event id (lowercase alphanumeric, ~26 chars). When the MCP call
    // fails or hits the auto-mode classifier, JARVIS replies with prose
    // explaining the block — without this check we'd accept that prose
    // as "msg_id" and mark the channel `sent`, silently swallowing the
    // failure instead of letting per-channel retry pick it up.
    let event_id = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .last()
        .ok_or_else(|| anyhow!("JARVIS returned no event id; raw reply: {raw}"))?;
    if !is_event_id_shape(event_id) {
        bail!(
            "JARVIS reply did not end with a calendar event id; got {:?}. Full reply: {raw}",
            event_id
        );
    }
    Ok(format!("calendar:{}", event_id))
}

fn is_event_id_shape(s: &str) -> bool {
    let len = s.len();
    (16..=64).contains(&len)
        && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// Char-safe truncation with ellipsis. SQL text in list output should
/// stay on one line; system_prompt values can be a paragraph.
fn truncate(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let head: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{head}…")
    } else {
        head
    }
}

/// First non-empty entry from a comma-separated env var. Used to pick
/// the default delivery target for WhatsApp channels — the first listed
/// group name (whatsapp-group/braindump) or the first listed DM JID.
fn first_csv_entry(env_var: &str) -> Option<String> {
    std::env::var(env_var)
        .ok()?
        .split(',')
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
