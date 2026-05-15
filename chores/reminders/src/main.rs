//! reminders — scheduled pings to Discord (and eventually WhatsApp).
//!
//! Subcommands:
//!   timesheet            daily end-of-day nudge to log hours (preset)
//!   add                  schedule an ad-hoc reminder
//!   list                 show pending reminders
//!   cancel <id>          mark a pending reminder cancelled
//!   due                  poll for due reminders and deliver them (cron)
//!
//! V1 delivers to Discord only. WhatsApp will land once we work out the
//! Baileys single-client constraint (Alfred already owns the auth).

mod store;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Local, Utc};
use clap::{Parser, Subcommand};
use nucleus_core::{config::Settings, diary, discord_sdk};
use std::path::Path;

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
    /// Daily end-of-day nudge to log hours.
    Timesheet,
    /// Schedule an ad-hoc reminder.
    Add {
        /// When to fire, as an ISO-8601 timestamp WITH timezone offset
        /// (e.g. "2026-05-14T16:45:00<offset>", where offset is "+HH:MM"
        /// or "-HH:MM"). The caller is responsible for converting natural
        /// language to ISO.
        #[arg(long)]
        at: String,
        /// The reminder body. What you want the bot to say to you.
        #[arg(long)]
        body: String,
        /// Where to deliver. Default: discord-home.
        #[arg(long, default_value = store::CHANNEL_DISCORD_HOME)]
        channel: String,
    },
    /// List pending reminders.
    List,
    /// Cancel a pending reminder by id.
    Cancel {
        #[arg(value_name = "ID")]
        id: i64,
    },
    /// Polling tick — find any due reminders and fire them. Run from
    /// launchd every minute.
    Due,
}

#[tokio::main]
async fn main() -> Result<()> {
    nucleus_core::init_tracing();
    let settings = Settings::load().context("loading settings")?;
    let workspace_root = std::env::current_dir()?;

    let cli = Cli::parse();
    match cli.command {
        Cmd::Timesheet => timesheet(&settings, &workspace_root).await,
        Cmd::Add { at, body, channel } => {
            add(&workspace_root, &at, &body, &channel).await
        }
        Cmd::List => list(&workspace_root).await,
        Cmd::Cancel { id } => cancel(&workspace_root, id).await,
        Cmd::Due => due(&settings, &workspace_root).await,
    }
}

async fn timesheet(settings: &Settings, workspace_root: &Path) -> Result<()> {
    if settings.discord.home_channel_id.is_empty() {
        bail!("DISCORD_HOME_CHANNEL_ID is not set; nothing to send to");
    }
    let mention = settings
        .discord
        .allowed_user_ids
        .first()
        .map(|id| format!("<@{}> ", id))
        .unwrap_or_default();
    let content = format!("{}⏰ End of day — time to log your hours.", mention);

    let msg_id = discord_sdk::send_announcement(
        &settings.discord.home_channel_id,
        &content,
    )
    .await?;
    tracing::info!("timesheet reminder sent (msg {})", msg_id);

    let _ = diary::record_observation(
        workspace_root,
        AGENT_NAME,
        "timesheet",
        &format!("posted reminder, message {}", msg_id),
        diary::Tag::Observation,
    );
    Ok(())
}

async fn add(workspace_root: &Path, at: &str, body: &str, channel: &str) -> Result<()> {
    let due_at = DateTime::parse_from_rfc3339(at)
        .map_err(|e| anyhow!("--at must be RFC3339 with offset (e.g. 2026-05-14T16:45:00+00:00): {e}"))?
        .with_timezone(&Utc);
    if !is_known_channel(channel) {
        bail!(
            "unknown channel {:?}; supported: discord-home, alfred, braindump",
            channel
        );
    }
    if body.trim().is_empty() {
        bail!("--body cannot be empty");
    }

    let pool = store::open(&workspace_root.join(DB_PATH)).await?;
    let id = store::insert(&pool, due_at, body, channel).await?;
    let local = due_at.with_timezone(&Local);
    tracing::info!(
        id = id,
        due_local = %local.format("%Y-%m-%d %H:%M %Z"),
        channel = channel,
        body_len = body.len(),
        "reminder scheduled"
    );
    // Print id on stdout so callers (Claude in a bot session) can capture it.
    println!("{}", id);
    Ok(())
}

async fn list(workspace_root: &Path) -> Result<()> {
    let pool = store::open(&workspace_root.join(DB_PATH)).await?;
    let rows = store::list_pending(&pool).await?;
    if rows.is_empty() {
        println!("(no pending reminders)");
        return Ok(());
    }
    for r in rows {
        let local = DateTime::parse_from_rfc3339(&r.due_at)
            .map(|d| d.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|_| r.due_at.clone());
        println!("#{:<4} {}  [{}]  {}", r.id, local, r.channel, r.body);
    }
    Ok(())
}

async fn cancel(workspace_root: &Path, id: i64) -> Result<()> {
    let pool = store::open(&workspace_root.join(DB_PATH)).await?;
    let cancelled = store::cancel(&pool, id).await?;
    if cancelled {
        tracing::info!(id = id, "cancelled reminder");
        println!("cancelled #{}", id);
    } else {
        bail!("#{} not found (or already fired/cancelled)", id);
    }
    Ok(())
}

async fn due(settings: &Settings, workspace_root: &Path) -> Result<()> {
    let pool = store::open(&workspace_root.join(DB_PATH)).await?;
    let now = Utc::now();
    let due_list = store::pending_due(&pool, now).await?;
    if due_list.is_empty() {
        // Quiet on the empty case — this runs every minute.
        return Ok(());
    }
    tracing::info!(count = due_list.len(), "delivering due reminders");

    let mention = settings
        .discord
        .allowed_user_ids
        .first()
        .map(|id| format!("<@{}> ", id))
        .unwrap_or_default();

    for r in due_list {
        match deliver(settings, workspace_root, &mention, &r).await {
            Ok(msg_id) => {
                store::mark_fired(&pool, r.id, &msg_id).await?;
                tracing::info!(id = r.id, msg = %msg_id, "fired");
                let _ = diary::record_observation(
                    workspace_root,
                    AGENT_NAME,
                    "fired",
                    &format!("reminder #{} ({}c) → {}", r.id, r.body.len(), r.channel),
                    diary::Tag::Observation,
                );
            }
            Err(e) => {
                // Stay pending — next tick retries. Log but don't bail; we
                // want the loop to continue for other reminders.
                tracing::warn!(id = r.id, err = %e, "deliver failed; will retry next tick");
            }
        }
    }
    Ok(())
}

async fn deliver(
    settings: &Settings,
    workspace_root: &Path,
    mention: &str,
    r: &store::Reminder,
) -> Result<String> {
    match r.channel.as_str() {
        store::CHANNEL_DISCORD_HOME => {
            if settings.discord.home_channel_id.is_empty() {
                bail!("DISCORD_HOME_CHANNEL_ID not set");
            }
            let body = format!("{}🔔 **Reminder:** {}", mention, r.body);
            discord_sdk::send_announcement(&settings.discord.home_channel_id, &body).await
        }
        store::CHANNEL_ALFRED | store::CHANNEL_BRAINDUMP => {
            // Pick the first configured group name for the requested role
            // and enqueue into whatsapp.db. Alfred's running process drains
            // every 5s and sends via its socket. The body is sent raw — no
            // mention prefix because WhatsApp doesn't render Discord-style
            // mentions, and the user is the only participant anyway.
            let env_var = match r.channel.as_str() {
                store::CHANNEL_ALFRED => "WHATSAPP_ALLOWED_GROUP_NAMES",
                store::CHANNEL_BRAINDUMP => "WHATSAPP_BRAINDUMP_GROUP_NAMES",
                _ => unreachable!(),
            };
            let target = first_group_name(env_var).ok_or_else(|| {
                anyhow!("channel {:?} requires {env_var} to be set with at least one group", r.channel)
            })?;
            let body = format!("🔔 *Reminder:* {}", r.body);
            let pool = store::open_whatsapp_db(&workspace_root.join(WHATSAPP_DB_PATH)).await?;
            let queue_id = store::enqueue_whatsapp(&pool, &target, &body, "reminders").await?;
            Ok(format!("whatsapp-queue#{}", queue_id))
        }
        other => bail!("unknown channel {:?}", other),
    }
}

/// Read a comma-separated env var (typically WHATSAPP_ALLOWED_GROUP_NAMES
/// or WHATSAPP_BRAINDUMP_GROUP_NAMES) and return the first entry, trimmed.
fn first_group_name(env_var: &str) -> Option<String> {
    std::env::var(env_var).ok()?.split(',').next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn is_known_channel(c: &str) -> bool {
    matches!(
        c,
        store::CHANNEL_DISCORD_HOME | store::CHANNEL_ALFRED | store::CHANNEL_BRAINDUMP
    )
}
