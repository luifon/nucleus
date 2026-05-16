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
    /// Schedule a reminder. Choose ONE of --at (one-shot) or --cron (recurring).
    Add {
        /// One-shot fire time. ISO-8601; offset optional (no offset = local TZ).
        #[arg(long, conflicts_with = "cron")]
        at: Option<String>,
        /// Standard 5-field cron expression, evaluated in NUCLEUS_TZ.
        #[arg(long, conflicts_with = "at")]
        cron: Option<String>,
        /// Reminder body.
        #[arg(long)]
        body: String,
        /// Comma-separated channels. Default: discord-home.
        #[arg(long, default_value = store::CHANNEL_DISCORD_HOME, value_delimiter = ',')]
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
        Cmd::Add { at, cron, body, channels } => {
            add(&workspace_root, at.as_deref(), cron.as_deref(), &body, &channels).await
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
    workspace_root: &Path,
    at: Option<&str>,
    cron: Option<&str>,
    body: &str,
    channels: &[String],
) -> Result<()> {
    if body.trim().is_empty() {
        bail!("--body cannot be empty");
    }
    if channels.is_empty() {
        bail!("--channels must list at least one destination");
    }

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
        body,
        &cron_expr,
        one_shot,
        next_fire_at,
        channels,
        "user",
    )
    .await?;
    let local = next_fire_at.with_timezone(&Local);
    tracing::info!(
        id = id,
        next_fire_local = %local.format("%Y-%m-%d %H:%M %Z"),
        cron = %cron_expr,
        one_shot = one_shot,
        channels = ?channels,
        body_len = body.len(),
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
        println!(
            "#{:<4} {}  [{}] {:>5}  ({})  {}{}",
            r.id,
            next,
            ch.join(","),
            kind,
            r.cron,
            r.body,
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
    println!("  body: {}", r.body);
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
        // Either advances the schedule (all channels terminal) or
        // leaves next_fire_at alone for the next tick to retry the
        // still-pending ones.
        if let Err(e) = store::advance_after_fire(&pool, reminder.id).await {
            tracing::warn!(id = reminder.id, err = %e, "advance_after_fire failed");
        }
    }
    Ok(())
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
        store::CHANNEL_ALFRED | store::CHANNEL_BRAINDUMP => {
            let env_var = match channel {
                store::CHANNEL_ALFRED => "WHATSAPP_ALLOWED_GROUP_NAMES",
                store::CHANNEL_BRAINDUMP => "WHATSAPP_BRAINDUMP_GROUP_NAMES",
                _ => unreachable!(),
            };
            let target = first_group_name(env_var).ok_or_else(|| {
                anyhow!(
                    "channel {:?} requires {env_var} to be set with at least one group",
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
/// lives on the trash account (the-trash-account) but the personal email is
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

    let persona_path = workspace_root.join("messaging/gmail/persona.md");
    let persona = tokio::fs::read_to_string(&persona_path)
        .await
        .with_context(|| format!("reading {}", persona_path.display()))?;
    let persona = config::substitute(&persona, &settings.identity);

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

    let event_id = raw
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .last()
        .ok_or_else(|| anyhow!("JARVIS returned no event id; raw reply: {raw}"))?
        .to_string();
    Ok(format!("calendar:{}", event_id))
}

fn first_group_name(env_var: &str) -> Option<String> {
    std::env::var(env_var)
        .ok()?
        .split(',')
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}
