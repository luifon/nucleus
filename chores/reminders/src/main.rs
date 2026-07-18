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

use reminders::store;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Local, Utc};
use clap::{Parser, Subcommand};
use nucleus_core::{
    config::{self, Settings},
    diary, discord_sdk,
    session_profile::{ProfileContext, SessionProfile},
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
        /// Short human-friendly name (≤60 chars recommended). Shown in
        /// the nucleus-dashboard /cron + /reminders surfaces. Optional
        /// for --body reminders (the body itself is descriptive enough),
        /// strongly recommended for --system-prompt reminders (the
        /// prompt reads as instructions, not as a label). ADR-015.
        #[arg(long)]
        title: Option<String>,
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
        /// ADR-024 condition watcher: shell command run (sh -c, 5s timeout)
        /// at each due tick. Exit 0 = fire; non-zero = skip silently and
        /// advance (cron) or keep watching every tick (one-shot). Optional
        /// stdout JSON {"context": "..."} is appended to the fire payload.
        #[arg(long)]
        condition: Option<String>,
        /// 'while-true' (default): fire on every truthy evaluation.
        /// 'change': fire only on a false→true transition.
        #[arg(long = "condition-mode", requires = "condition")]
        condition_mode: Option<String>,
    },
    /// Set (or clear with empty string) the title on an existing reminder.
    SetTitle {
        #[arg(value_name = "ID")]
        id: i64,
        #[arg(value_name = "TITLE")]
        title: String,
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
        Cmd::Add { title, at, cron, body, system_prompt, channels, condition, condition_mode } => {
            add(
                &settings,
                &workspace_root,
                title.as_deref(),
                at.as_deref(),
                cron.as_deref(),
                body.as_deref(),
                system_prompt.as_deref(),
                channels,
                condition.as_deref(),
                condition_mode.as_deref(),
            )
            .await
        }
        Cmd::SetTitle { id, title } => set_title(&workspace_root, id, &title).await,
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

#[allow(clippy::too_many_arguments)]
async fn add(
    settings: &Settings,
    workspace_root: &Path,
    title: Option<&str>,
    at: Option<&str>,
    cron: Option<&str>,
    body: Option<&str>,
    system_prompt: Option<&str>,
    channels: Vec<String>,
    condition: Option<&str>,
    condition_mode: Option<&str>,
) -> Result<()> {
    // ADR-024: validate the watcher args before touching the DB.
    let condition = match (condition, condition_mode) {
        (None, _) => None,
        (Some(cmd), mode) => {
            if cmd.trim().is_empty() {
                bail!("--condition cannot be empty");
            }
            let mode = mode.unwrap_or("while-true");
            if !matches!(mode, "while-true" | "change") {
                bail!("--condition-mode must be 'while-true' or 'change', got {mode:?}");
            }
            Some((cmd, mode))
        }
    };

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
        title.filter(|t| !t.trim().is_empty()),
        &body_stored,
        &cron_expr,
        one_shot,
        next_fire_at,
        &channels,
        "user",
        system_prompt_stored.as_deref(),
        condition,
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
        // Title (if set) takes precedence as the display name; the
        // body/system_prompt becomes the secondary line.
        let payload_display = match (r.title.as_deref(), r.system_prompt.as_deref()) {
            (Some(t), Some(_)) => format!("🪄 {}", t),
            (Some(t), None)    => t.to_string(),
            (None, Some(sp))   => format!("🪄 {}", truncate(sp, 60)),
            (None, None)       => r.body.clone(),
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
    if let Some(cmd) = r.condition_cmd.as_deref() {
        println!(
            "  ⛩ condition [{}]: {}  (last eval: {} at {})",
            r.condition_mode.as_deref().unwrap_or("while-true"),
            cmd,
            match r.condition_state {
                Some(true) => "true",
                Some(false) => "false",
                None => "never",
            },
            r.condition_checked_at.as_deref().unwrap_or("—"),
        );
    }
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

async fn set_title(workspace_root: &Path, id: i64, title: &str) -> Result<()> {
    let pool = open_pool(workspace_root).await?;
    let stored = if title.trim().is_empty() { None } else { Some(title) };
    let rows = store::set_title(&pool, id, stored).await?;
    if rows == 0 {
        bail!("#{} not found", id);
    }
    match stored {
        Some(t) => println!("#{} title set to {:?}", id, t),
        None => println!("#{} title cleared", id),
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

        // ADR-024 condition gate: a watcher command decides whether this
        // due tick actually fires. Gated ticks are success-neutral (no
        // channel fires, no retry burn); the evaluation is recorded in
        // place on the reminder row.
        let mut reminder = reminder;
        if let Some(cmd) = reminder.condition_cmd.clone() {
            match eval_condition(&cmd).await {
                Ok(eval) => {
                    let prev = reminder.condition_state;
                    store::record_condition_eval(&pool, reminder.id, eval.truthy).await?;
                    if !condition_should_fire(reminder.condition_mode.as_deref(), eval.truthy, prev)
                    {
                        tracing::debug!(
                            id = reminder.id,
                            truthy = eval.truthy,
                            "condition gated; not firing"
                        );
                        if let Err(e) = store::advance_after_gate(&pool, reminder.id).await {
                            tracing::warn!(id = reminder.id, err = %e, "advance_after_gate failed");
                        }
                        continue;
                    }
                    if let Some(ctx) = eval.context {
                        // Hand the watcher's evidence to the fire so the
                        // session doesn't re-derive it.
                        let suffix = format!("\n\n[condition context] {ctx}");
                        match reminder.system_prompt.as_mut() {
                            Some(sp) => sp.push_str(&suffix),
                            None => reminder.body.push_str(&suffix),
                        }
                    }
                }
                Err(e) => {
                    // A broken watch IS a failure (unlike a gated tick):
                    // record it, then stop the bleeding — cron advances to
                    // its next match; a one-shot would re-fail every tick
                    // forever, so it gets paused for the operator to fix.
                    let err = format!("condition failed: {e:#}");
                    tracing::warn!(id = reminder.id, err = %err, "condition watcher broken");
                    store::record_channel_fire(
                        &pool,
                        reminder.id,
                        "condition",
                        fired_at,
                        store::FireOutcome { success: false, msg_id: None, error: Some(&err) },
                    )
                    .await?;
                    if reminder.one_shot {
                        let _ = store::pause(&pool, reminder.id, None).await;
                    } else if let Err(e) = store::advance_after_gate(&pool, reminder.id).await {
                        tracing::warn!(id = reminder.id, err = %e, "advance_after_gate failed");
                    }
                    continue;
                }
            }
        }

        if reminder.system_prompt.is_some() {
            // Skill-fire: spawn one session, then forward its reply to
            // each channel as the actual user-visible output. The
            // session has no built-in posting tool, so the reminders
            // worker is the bridge (see persona.md for the contract).
            match deliver_skill_fire(settings, workspace_root, &reminder, &channels).await {
                Ok(SkillFireResult { msg_id, reply }) if is_silent_reply(&reply) => {
                    // Reply-gated delivery (ADR-026): the session judged
                    // there is nothing worth the operator's attention.
                    // Record a successful fire on every channel, deliver
                    // nothing anywhere.
                    tracing::info!(
                        id = reminder.id,
                        msg = %msg_id,
                        "skill-fire ok — silent reply, delivery suppressed"
                    );
                    let silent_msg_id = format!("{}|silent", msg_id);
                    for ch in &channels {
                        store::record_channel_fire(
                            &pool,
                            reminder.id,
                            &ch.channel,
                            fired_at,
                            store::FireOutcome {
                                success: true,
                                msg_id: Some(&silent_msg_id),
                                error: None,
                            },
                        )
                        .await?;
                    }
                    let _ = diary::record_observation(
                        workspace_root,
                        AGENT_NAME,
                        "fired",
                        &format!("reminder #{} skill-fire → silent (HEARTBEAT_OK)", reminder.id),
                        diary::Tag::Observation,
                    );
                }
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
                        // The skill-fire session succeeded; the forward to
                        // this channel may or may not have. We preserve the
                        // skill-fire:xxx prefix in msg_id either way so the
                        // session-log → reminder_fires join still works, but
                        // success must reflect the FORWARD outcome — a
                        // failed channel delivery is not a successful fire
                        // for that channel, even if the underlying session
                        // produced a reply.
                        let (recorded_msg_id, forward_err) = match forward {
                            Ok(fw_msg) => (format!("{}|fwd:{}", msg_id, fw_msg), None),
                            Err(e) => {
                                // {:#} keeps the whole anyhow chain — the
                                // outer context alone ("deliver to …") is
                                // useless for triage.
                                let err_str = format!("{e:#}");
                                tracing::warn!(
                                    id = reminder.id,
                                    channel = %ch.channel,
                                    err = %err_str,
                                    "skill-fire reply forward failed"
                                );
                                (
                                    format!("{}|fwd-err:{}", msg_id, truncate(&err_str, 80)),
                                    Some(err_str),
                                )
                            }
                        };
                        store::record_channel_fire(
                            &pool,
                            reminder.id,
                            &ch.channel,
                            fired_at,
                            store::FireOutcome {
                                success: forward_err.is_none(),
                                msg_id: Some(&recorded_msg_id),
                                error: forward_err.as_deref(),
                            },
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
                    // {:#} keeps the whole anyhow chain — persisting only
                    // the outer context ("fire session for reminder #N")
                    // made 2026-07-18's failure untriageable after the fact.
                    let err = format!("{e:#}");
                    tracing::warn!(
                        id = reminder.id,
                        err = %err,
                        "skill-fire failed"
                    );
                    // Outer-error alert (ADR-008 failure handling layer 2),
                    // but retry-aware: a channel with budget left re-spawns
                    // the fire next tick, so alerting on a non-final attempt
                    // is noise — the operator gets a ⚠ followed minutes
                    // later by the successful retry's deliverable (observed
                    // with #47, 2026-07-18). Alert only the channels this
                    // failure exhausts; record every failure regardless.
                    for ch in &channels {
                        let attempt = ch.attempts + 1;
                        if alert_on_this_attempt(ch.attempts) {
                            let alert_reminder = store::Reminder {
                                body: format!(
                                    "⚠️ Reminder #{} fire failed (attempt {}/{}, giving up): {}",
                                    reminder.id,
                                    attempt,
                                    store::MAX_ATTEMPTS,
                                    truncate(&err, 200)
                                ),
                                ..reminder.clone()
                            };
                            let _ = deliver(
                                settings,
                                workspace_root,
                                &mention,
                                &alert_reminder,
                                &ch.channel,
                            )
                            .await;
                        }
                        store::record_channel_fire(
                            &pool,
                            reminder.id,
                            &ch.channel,
                            fired_at,
                            store::FireOutcome {
                                success: false,
                                msg_id: None,
                                error: Some(&err),
                            },
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
                            store::FireOutcome {
                                success: true,
                                msg_id: Some(&msg_id),
                                error: None,
                            },
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
                        // {:#} keeps the whole anyhow chain for triage.
                        let err = format!("{e:#}");
                        store::record_channel_fire(
                            &pool,
                            reminder.id,
                            &ch.channel,
                            fired_at,
                            store::FireOutcome {
                                success: false,
                                msg_id: None,
                                error: Some(&err),
                            },
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

    // ADR-020: SessionProfile::one_shot_agentic carries the posture this
    // path needs — Settings disallowed_tools, await_turn_complete (the model
    // goes quiet between tool calls; quiescence alone tore the session down
    // mid-task, the DSU skill-fire failures, 2026-05-26), 300s ceiling
    // for Playwright-driven skills. Bounded by the per-tick file lock so we
    // don't overlap. No allowed_tools pre-approval at this layer — each
    // skill's own `allowed-tools` frontmatter handles tool gating.
    let outcome = SessionProfile::one_shot_agentic(&ProfileContext {
        workspace_root,
        claude: &settings.claude,
        tmux_session: "nucleus-reminders-fire",
        agent_label: "reminders-fire",
    })
    .system_prompt(persona)
    .window_name(format!("fire-{}", r.id))
    .run_one_shot(&ask_payload)
    .await
    .with_context(|| format!("fire session for reminder #{}", r.id))?;

    if outcome.reply.trim().is_empty() {
        bail!("session returned an empty reply (treated as failure)");
    }

    // Durable guard against the narration-leak failure mode. `ask()` returns
    // the last assistant *text* block regardless of what followed it, so a
    // session that crashed or was cut off mid-action hands back a stale
    // mid-process line ("Let me click…", "I accidentally opened…") instead of
    // a finished deliverable — and we'd forward THAT to the operator as their
    // standup (a DSU skill-fire did, 2026-05-26). Only forward when
    // the session ended on a clean assistant text turn; otherwise fail into
    // the ⚠️ alert path so the operator gets a "fire failed" notice, never
    // a leaked internal monologue.
    if !outcome.ended_clean {
        bail!(
            "session ended mid-action (last assistant output was a tool call, not a \
             final reply) — suppressing forward to avoid leaking narration as the post"
        );
    }

    // Second half of the narration guard: even a clean final text block can
    // carry thinking-out-loud ABOVE the deliverable ("Pesquisa concluída —
    // compondo a mensagem final." posted to WhatsApp, 2026-07-18). The persona
    // requires the post to start at a `===POST===` marker line; strip the
    // marker and anything before it. No marker → forward as-is (the contract
    // is belt, this is suspenders — never fail a fire over formatting).
    let reply = strip_post_marker(&outcome.reply);
    if reply.len() != outcome.reply.trim().len() {
        tracing::info!(
            reminder_id = r.id,
            stripped_chars = outcome.reply.trim().len().saturating_sub(reply.len()),
            "stripped pre-marker narration from skill-fire reply"
        );
    }

    let _ = diary::record_observation(
        workspace_root,
        AGENT_NAME,
        "skill-fire",
        &format!("reminder #{}: {}", r.id, truncate(&reply, 200)),
        diary::Tag::Observation,
    );

    Ok(SkillFireResult {
        msg_id: format!("skill-fire:{}", outcome.session_id),
        reply,
    })
}

/// Marker the fire persona puts at the start of the ready-to-send post.
const POST_MARKER: &str = "===POST===";

/// Return everything after the first line that IS the marker (trimmed line
/// equality, so narration merely *mentioning* the marker inline doesn't
/// truncate the post). Without a marker, the trimmed reply passes through
/// unchanged.
fn strip_post_marker(reply: &str) -> String {
    let mut before = true;
    let mut out: Vec<&str> = Vec::new();
    for line in reply.lines() {
        if before && line.trim() == POST_MARKER {
            before = false;
            continue;
        }
        if !before {
            out.push(line);
        }
    }
    let body = out.join("\n");
    let body = body.trim();
    // Marker missing OR marker with an empty body → pass the trimmed original
    // through; a fire must never be emptied by formatting.
    if body.is_empty() {
        reply.trim().to_string()
    } else {
        body.to_string()
    }
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
/// Stale-lock recovery: if the lockfile is older than `STALE_AFTER` we
/// assume the prior process crashed (SIGKILL leaves the file behind
/// because Drop didn't run) and reclaim it.
///
/// Heartbeat (ADR-020): a live holder rewrites the lockfile every
/// `HEARTBEAT_EVERY`, refreshing its mtime — so staleness means *dead*,
/// not *slow*. Before this, a fire that outlived the 10-minute window
/// (Playwright-driven skills, anything past max_wait + spawn/close
/// overhead) had its lock reclaimed by the next tick and the same
/// reminder fired twice. If the heartbeat task panics or is starved,
/// beats stop and we fail open to the pre-existing stale-reclaim path —
/// never a deadlock.
struct TickLock {
    path: std::path::PathBuf,
    heartbeat: tokio::task::JoinHandle<()>,
}

/// Lockfile age past which a holder is presumed dead. ~9 missed beats
/// of tolerance against system sleep / disk hiccups.
const TICK_LOCK_STALE_AFTER: Duration = Duration::from_secs(10 * 60);
const TICK_LOCK_HEARTBEAT_EVERY: Duration = Duration::from_secs(60);

impl TickLock {
    fn try_acquire(path: std::path::PathBuf) -> Result<Option<Self>> {
        Self::try_acquire_with(path, TICK_LOCK_STALE_AFTER, TICK_LOCK_HEARTBEAT_EVERY)
    }

    fn try_acquire_with(
        path: std::path::PathBuf,
        stale_after: Duration,
        heartbeat_every: Duration,
    ) -> Result<Option<Self>> {
        use std::fs::{OpenOptions, metadata};
        use std::io::ErrorKind;
        use std::io::Write;
        use std::time::SystemTime;

        if let Ok(meta) = metadata(&path) {
            let stale = meta
                .modified()
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .map(|d| d > stale_after)
                .unwrap_or(false);
            if stale {
                tracing::warn!(
                    path = %path.display(),
                    "stale reminders-tick lockfile (no heartbeat for >{}s); reclaiming",
                    stale_after.as_secs()
                );
                let _ = std::fs::remove_file(&path);
            }
        }

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                // PID + timestamp for `cat memory/reminders-tick.lock`
                // debuggability; content is informational only.
                let _ = writeln!(f, "{} {}", std::process::id(), Utc::now().to_rfc3339());

                let hb_path = path.clone();
                let heartbeat = tokio::spawn(async move {
                    let mut tick = tokio::time::interval(heartbeat_every);
                    tick.tick().await; // consume the immediate first tick
                    loop {
                        tick.tick().await;
                        // Rewrite = mtime refresh. create(true), NOT
                        // create_new: if the file vanished (operator rm),
                        // recreating re-asserts the lock we still hold.
                        let stamp =
                            format!("{} {}\n", std::process::id(), Utc::now().to_rfc3339());
                        if let Err(e) = std::fs::write(&hb_path, stamp) {
                            tracing::warn!("tick-lock heartbeat write failed: {e}");
                        }
                    }
                });

                Ok(Some(TickLock { path, heartbeat }))
            }
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
        // Abort BEFORE removing so the heartbeat can't resurrect the file
        // after release.
        self.heartbeat.abort();
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
        store::CHANNEL_WHATSAPP_DM => {
            let target = first_csv_entry("WHATSAPP_ALLOWED_DM_JIDS").ok_or_else(|| {
                anyhow!(
                    "channel {:?} requires WHATSAPP_ALLOWED_DM_JIDS to be set with at least one entry",
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
    // need conversational continuity. ADR-020: one_shot_mcp pre-approves the
    // create_event MCP call as a constructor arg. Without it, the auto-mode
    // classifier blocks invites sent to "external" addresses — but our
    // entire design point is to send invites to NUCLEUS_PERSONAL_EMAIL; the
    // persona already constrains who may be addressed. tmux session is the
    // venue (Rule 7 / ADR-016) — shared with gmail-metabolism; JARVIS is the
    // persona, not the code identity.
    let outcome = SessionProfile::one_shot_mcp(
        &ProfileContext {
            workspace_root,
            claude: &settings.claude,
            tmux_session: "nucleus-gmail",
            agent_label: "calendar-fire",
        },
        vec!["mcp__claude_ai_Google_Calendar__create_event".into()],
    )
    .system_prompt(persona)
    .window_name(format!("cal-{}", r.id))
    .max_wait(Duration::from_secs(60))
    .run_one_shot(&prompt)
    .await
    .context("JARVIS calendar create_event")?;
    let raw = outcome.reply;

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

/// Outcome of one ADR-024 condition evaluation.
struct CondEval {
    truthy: bool,
    /// Parsed from stdout JSON {"context": "..."} on a truthy exit —
    /// evidence handed to the fire payload. Non-JSON stdout is ignored.
    context: Option<String>,
}

/// Run a condition watcher command (sh -c, hard 5s timeout). Exit 0 =
/// truthy. Spawn failure or timeout is an Err — a broken watch, distinct
/// from a false condition.
async fn eval_condition(cmd: &str) -> Result<CondEval> {
    let out = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow!("timed out after 5s"))?
    .context("spawn failed")?;
    let truthy = out.status.success();
    let context = if truthy {
        serde_json::from_slice::<serde_json::Value>(&out.stdout)
            .ok()
            .and_then(|v| v.get("context").and_then(|c| c.as_str()).map(str::to_string))
    } else {
        None
    };
    Ok(CondEval { truthy, context })
}

/// The ADR-024 fire decision. `while-true` (the default when mode is
/// None/unknown) fires on every truthy evaluation; `change` only on a
/// false→true transition — a persistently-true condition alerts once.
/// `prev` is the state recorded BEFORE this evaluation (None = never
/// evaluated, which counts as "was false" so a first truthy eval fires).
fn condition_should_fire(mode: Option<&str>, truthy: bool, prev: Option<bool>) -> bool {
    if !truthy {
        return false;
    }
    match mode {
        Some("change") => prev != Some(true),
        _ => true,
    }
}

/// Reply-gated delivery (ADR-026): a skill-fire session that has nothing
/// worth the operator's attention replies exactly `HEARTBEAT_OK` (after
/// marker-strip) and the worker suppresses delivery on every channel while
/// still recording a successful fire. Strict equality modulo surrounding
/// whitespace — a report that merely *mentions* the token must deliver.
fn is_silent_reply(reply: &str) -> bool {
    reply.trim() == "HEARTBEAT_OK"
}

/// A skill-fire failure alerts a channel only when it exhausts that
/// channel's retry budget — with attempts left, next tick re-spawns the
/// fire, and a ⚠ followed minutes later by the successful retry's
/// deliverable is pure noise (observed with #47, 2026-07-18). Takes the
/// channel's attempt count BEFORE this failure is recorded.
fn alert_on_this_attempt(prior_attempts: i64) -> bool {
    prior_attempts + 1 >= store::MAX_ATTEMPTS
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

#[cfg(test)]
mod condition_tests {
    use super::{condition_should_fire, eval_condition};

    /// A false condition never fires, in any mode, whatever the history.
    #[test]
    fn false_never_fires() {
        for mode in [None, Some("while-true"), Some("change")] {
            for prev in [None, Some(false), Some(true)] {
                assert!(!condition_should_fire(mode, false, prev));
            }
        }
    }

    /// while-true (and unknown modes, defensively) fire on every truthy
    /// evaluation regardless of the previous state.
    #[test]
    fn while_true_fires_on_every_truthy_eval() {
        for mode in [None, Some("while-true"), Some("garbage")] {
            for prev in [None, Some(false), Some(true)] {
                assert!(condition_should_fire(mode, true, prev));
            }
        }
    }

    /// change fires only on the false→true transition; never-evaluated
    /// counts as false so the first truthy eval fires.
    #[test]
    fn change_fires_only_on_transition() {
        assert!(condition_should_fire(Some("change"), true, None));
        assert!(condition_should_fire(Some("change"), true, Some(false)));
        assert!(!condition_should_fire(Some("change"), true, Some(true)));
    }

    #[tokio::test]
    async fn eval_condition_truthiness_and_context() {
        assert!(eval_condition("true").await.unwrap().truthy);
        assert!(!eval_condition("false").await.unwrap().truthy);
        // context comes only from valid JSON stdout on truthy exits
        let e = eval_condition(r#"echo '{"context":"queue depth 14"}'"#).await.unwrap();
        assert_eq!(e.context.as_deref(), Some("queue depth 14"));
        let e = eval_condition("echo not-json").await.unwrap();
        assert!(e.truthy && e.context.is_none());
        // a hung watcher is an Err (broken watch), not a false condition
        assert!(eval_condition("sleep 10").await.is_err());
    }
}

#[cfg(test)]
mod silent_reply_tests {
    use super::is_silent_reply;

    #[test]
    fn exact_token_is_silent_modulo_whitespace() {
        assert!(is_silent_reply("HEARTBEAT_OK"));
        assert!(is_silent_reply("  HEARTBEAT_OK\n"));
    }

    /// Anything beyond the bare token must deliver — a report that merely
    /// mentions the token, wrong case, or an empty reply (empty is an
    /// error shape, not a silence request).
    #[test]
    fn non_bare_replies_deliver() {
        assert!(!is_silent_reply("HEARTBEAT_OK — but disk is low"));
        assert!(!is_silent_reply("All quiet.\nHEARTBEAT_OK"));
        assert!(!is_silent_reply("heartbeat_ok"));
        assert!(!is_silent_reply(""));
    }
}

#[cfg(test)]
mod alert_gating_tests {
    use super::alert_on_this_attempt;
    use super::store::MAX_ATTEMPTS;

    /// Attempts 1..MAX-1 stay silent (retry will re-spawn); the attempt
    /// that exhausts the budget alerts. Pinned to MAX_ATTEMPTS so a
    /// budget change can't silently open an alert gap.
    #[test]
    fn alerts_only_on_the_budget_exhausting_attempt() {
        for prior in 0..MAX_ATTEMPTS - 1 {
            assert!(
                !alert_on_this_attempt(prior),
                "attempt {} of {MAX_ATTEMPTS} must be silent",
                prior + 1
            );
        }
        assert!(alert_on_this_attempt(MAX_ATTEMPTS - 1));
        // Defensive: attempts beyond the budget (shouldn't occur) still alert.
        assert!(alert_on_this_attempt(MAX_ATTEMPTS + 3));
    }
}

#[cfg(test)]
mod strip_post_marker_tests {
    use super::strip_post_marker;

    #[test]
    fn strips_narration_before_marker() {
        let r = "Pesquisa concluída — compondo a mensagem final.\n\n===POST===\n**Servidor caseiro — 4 opções:**\n1. Beelink";
        assert_eq!(strip_post_marker(r), "**Servidor caseiro — 4 opções:**\n1. Beelink");
    }

    #[test]
    fn no_marker_passes_through_trimmed() {
        assert_eq!(strip_post_marker("  plain reply\nline 2  \n"), "plain reply\nline 2");
    }

    #[test]
    fn marker_with_surrounding_whitespace_matches() {
        assert_eq!(strip_post_marker("noise\n  ===POST===  \npost body"), "post body");
    }

    #[test]
    fn inline_mention_does_not_truncate() {
        let r = "o marcador ===POST=== é usado assim\nconteúdo";
        assert_eq!(strip_post_marker(r), r.trim());
    }

    #[test]
    fn marker_first_line_keeps_everything_after() {
        assert_eq!(strip_post_marker("===POST===\nbody"), "body");
    }

    #[test]
    fn marker_with_empty_body_falls_back_to_original() {
        let r = "narration\n===POST===\n\n";
        assert_eq!(strip_post_marker(r), r.trim());
    }
}

#[cfg(test)]
mod tick_lock_tests {
    use super::*;

    fn tmp_lock_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("nucleus-ticklock-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        let _ = std::fs::remove_file(&p);
        p
    }

    #[tokio::test]
    async fn second_acquire_is_refused_while_held() {
        let path = tmp_lock_path("held.lock");
        let lock = TickLock::try_acquire_with(
            path.clone(),
            Duration::from_secs(600),
            Duration::from_secs(3600),
        )
        .unwrap()
        .expect("first acquire succeeds");
        assert!(
            TickLock::try_acquire_with(path, Duration::from_secs(600), Duration::from_secs(3600))
                .unwrap()
                .is_none(),
            "second acquire must be refused"
        );
        drop(lock);
    }

    #[tokio::test]
    async fn heartbeat_keeps_lock_fresh_past_stale_window() {
        let path = tmp_lock_path("heartbeat.lock");
        // Stale window 2s, heartbeat 300ms: without the heartbeat the lock
        // would be reclaimable after 2s — the beats must prevent that.
        let lock = TickLock::try_acquire_with(
            path.clone(),
            Duration::from_secs(2),
            Duration::from_millis(300),
        )
        .unwrap()
        .expect("first acquire succeeds");
        tokio::time::sleep(Duration::from_millis(2_500)).await;
        assert!(
            TickLock::try_acquire_with(
                path,
                Duration::from_secs(2),
                Duration::from_secs(3600)
            )
            .unwrap()
            .is_none(),
            "live holder must not be reclaimed: heartbeat keeps mtime fresh"
        );
        drop(lock);
    }

    #[tokio::test]
    async fn dead_holder_is_reclaimed_after_stale_window() {
        let path = tmp_lock_path("dead.lock");
        let lock = TickLock::try_acquire_with(
            path.clone(),
            Duration::from_millis(800),
            Duration::from_secs(3600), // no beats inside the window
        )
        .unwrap()
        .expect("first acquire succeeds");
        // Simulate SIGKILL: stop the heartbeat and skip Drop so the file
        // stays behind with an aging mtime.
        lock.heartbeat.abort();
        std::mem::forget(lock);
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        let reclaimed = TickLock::try_acquire_with(
            path.clone(),
            Duration::from_millis(800),
            Duration::from_secs(3600),
        )
        .unwrap();
        assert!(reclaimed.is_some(), "stale lock from a dead holder must be reclaimed");
        drop(reclaimed);
    }
}
