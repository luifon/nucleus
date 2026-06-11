//! Nucleus Discord bot. Persona is resolved at spawn via
//! `NUCLEUS_PERSONA_DISCORD=<slug>` → `personas/<slug>.md` (see ADR-009).
//!
//! Maintains a per-channel `SessionPool` against long-lived interactive
//! `claude` processes inside tmux. Persona injected via --append-system-prompt
//! at spawn. See ADR-001 (architecture), ADR-003 (security), ADR-004 (diary).
//! Watch live: `tmux attach -t nucleus-discord`.

use anyhow::{Context, Result};
use nucleus_core::{
    claude_session::{AskOptions, SessionPool},
    config::Settings,
    db, diary,
    session_profile::{self, ProfileContext},
};
use serenity::all::{
    Channel, ChannelId, Command, CommandDataOptionValue, CommandInteraction,
    CommandOptionType, CreateCommand, CreateCommandOption, CreateInteractionResponse,
    CreateInteractionResponseFollowup, CreateInteractionResponseMessage, CreateMessage,
    EventHandler, GatewayIntents, Interaction, Message, MessageFlags, Ready, UserId,
};
use serenity::async_trait;
use serenity::client::Client;
use serenity::http::Http;
use sqlx::SqlitePool;
use std::path::PathBuf;
use std::sync::Arc;

const AGENT_NAME: &str = "discord";
const MAX_DISCORD_CHARS: usize = 2000;
const DB_PATH: &str = "memory/discord.db";

/// Per-message context handed to the worker that calls claude.
struct Job {
    channel_id: ChannelId,
    is_dm: bool,
    author_id: UserId,
    author_name: String,
    prompt: String,
}

struct Handler {
    pool: SqlitePool,
    sessions: Arc<SessionPool>,
    /// Profile-derived per-turn ask options (ADR-020) — interactive pool
    /// semantics (quiescence-based doneness).
    ask_options: AskOptions,
    settings: Settings,
    workspace_root: PathBuf,
    bot_user_id: tokio::sync::OnceCell<UserId>,
    home_channel_id: u64,
}

impl Handler {
    /// True if the message author is in the allowlist.
    fn is_allowed(&self, author: &UserId) -> bool {
        let id = author.get().to_string();
        self.settings.discord.allowed_user_ids.iter().any(|a| a == &id)
    }

    /// Decide whether this message should trigger the bot.
    /// Returns the cleaned prompt (mentions stripped) if so.
    async fn should_handle(&self, msg: &Message, http: &Http) -> Option<(bool, String)> {
        let bot_id = *self.bot_user_id.get()?;
        if msg.author.bot {
            return None;
        }
        if !self.is_allowed(&msg.author.id) {
            return None;
        }

        // DM detection: fetch the channel and check kind.
        let is_dm = match msg.channel(http).await {
            Ok(Channel::Private(_)) => true,
            _ => false,
        };

        if is_dm {
            if !self.settings.discord.dms_always_respond {
                return None;
            }
            return Some((true, msg.content.clone()));
        }

        // Channel: mention-only when configured.
        if self.settings.discord.mention_only_in_channels {
            let mentioned = msg.mentions.iter().any(|u| u.id == bot_id);
            if !mentioned {
                return None;
            }
        }

        // Strip the bot's @mention from the body.
        let cleaned = msg
            .content
            .replace(&format!("<@{}>", bot_id.get()), "")
            .replace(&format!("<@!{}>", bot_id.get()), "")
            .trim()
            .to_string();

        Some((false, cleaned))
    }

    async fn process(&self, http: Arc<Http>, job: Job) -> Result<()> {
        let _typing = job.channel_id.start_typing(&http);

        // Persisted session_id for graceful restart — None means first ever
        // message for this channel, so the pool will spawn a fresh session.
        let resume = lookup_session(&self.pool, job.channel_id.get()).await?;
        let chat_key = job.channel_id.get().to_string();

        // Frame the prompt so Claude knows the channel/user context.
        let framed = format!(
            "[Discord {} — from {}]\n\n{}",
            if job.is_dm { "DM".to_string() } else { format!("channel {}", job.channel_id.get()) },
            job.author_name,
            job.prompt
        );

        let result = self
            .sessions
            .ask(&chat_key, &framed, resume.clone(), self.ask_options.clone())
            .await?;

        // Persist (or refresh) the session id so a bot restart can resume.
        save_session(
            &self.pool,
            job.channel_id.get(),
            &result.session_id,
            resume.is_none(),
        )
        .await?;

        let reply = if result.reply.trim().is_empty() {
            "(no response)".to_string()
        } else {
            result.reply.clone()
        };
        send_chunked(&http, job.channel_id, &reply).await?;

        // On-the-fly skill review (ADR-017) — detached, fire-and-forget, never
        // blocks or fails the reply we just sent.
        if result.review_due {
            nucleus_core::skills::fire_skill_review(
                &self.workspace_root,
                "discord",
                &chat_key,
                &result.transcript_path,
            );
        }

        // Diary auto-append.
        let summary = format!(
            "{} — replied in {:.1}s ({} session {})",
            if job.is_dm { "DM".to_string() } else { format!("#channel {}", job.channel_id.get()) },
            result.elapsed.as_secs_f64(),
            if result.was_cold_spawn { "cold" } else { "warm" },
            result.session_id,
        );
        let _ = diary::record_observation(
            &self.workspace_root,
            AGENT_NAME,
            if job.is_dm { "DM" } else { "channel" },
            &summary,
            diary::Tag::Observation,
        );
        Ok(())
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: serenity::client::Context, ready: Ready) {
        let _ = self.bot_user_id.set(ready.user.id);
        tracing::info!(
            "discord: connected as {} (id={}) — guilds={}",
            ready.user.name, ready.user.id.get(), ready.guilds.len()
        );
        // Register global slash commands. Globals can take ~1h to propagate;
        // for instant testing, register per-guild instead.
        let commands = vec![
            CreateCommand::new("status")
                .description("Show Nucleus services status"),
            CreateCommand::new("news")
                .description("Today's notable news items")
                .add_option(
                    CreateCommandOption::new(CommandOptionType::Integer, "limit", "How many to show (default 5)")
                        .required(false),
                ),
            CreateCommand::new("remember")
                .description("Save a fact to long-term shared memory")
                .add_option(
                    CreateCommandOption::new(CommandOptionType::String, "fact", "What to remember")
                        .required(true),
                )
                .add_option(
                    CreateCommandOption::new(CommandOptionType::String, "kind", "user | feedback | project | reference")
                        .required(false),
                ),
            CreateCommand::new("forget")
                .description("Delete a memory by name")
                .add_option(
                    CreateCommandOption::new(CommandOptionType::String, "name", "Memory name (kebab-case slug)")
                        .required(true),
                ),
        ];
        match Command::set_global_commands(&ctx.http, commands).await {
            Ok(set) => tracing::info!("discord: registered {} global slash commands", set.len()),
            Err(e) => tracing::warn!("discord: slash command registration failed: {}", e),
        }
        let _ = diary::record_observation(
            &self.workspace_root,
            AGENT_NAME,
            "boot",
            &format!("Connected as {} (id={})", ready.user.name, ready.user.id.get()),
            diary::Tag::Observation,
        );
    }

    async fn interaction_create(&self, ctx: serenity::client::Context, interaction: Interaction) {
        let Some(cmd) = interaction.command() else { return; };
        if !self.is_allowed(&cmd.user.id) {
            let _ = cmd.create_response(&ctx.http, CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content(":no_entry: not on the allowlist")
                    .ephemeral(true),
            )).await;
            return;
        }
        // Defer so we have time to do work.
        let _ = cmd.create_response(&ctx.http, CreateInteractionResponse::Defer(
            CreateInteractionResponseMessage::new().ephemeral(true),
        )).await;

        let reply = match cmd.data.name.as_str() {
            "status" => handle_status().await,
            "news" => handle_news(&cmd).await,
            "remember" => handle_remember(&cmd, &self.settings).await,
            "forget" => handle_forget(&cmd).await,
            other => Ok(format!(":question: unknown command `{}`", other)),
        };
        let content = match reply {
            Ok(c) => c,
            Err(e) => format!(":warning: {}", e),
        };
        let _ = cmd.create_followup(&ctx.http, CreateInteractionResponseFollowup::new()
            .content(content)
            .flags(MessageFlags::SUPPRESS_EMBEDS)
            .ephemeral(true)
        ).await;
    }

    async fn message(&self, ctx: serenity::client::Context, msg: Message) {
        let Some((is_dm, prompt)) = self.should_handle(&msg, &ctx.http).await else {
            return;
        };
        if prompt.is_empty() {
            return;
        }
        let job = Job {
            channel_id: msg.channel_id,
            is_dm,
            author_id: msg.author.id,
            author_name: msg.author.name.clone(),
            prompt,
        };
        let http = ctx.http.clone();
        match self.process(http.clone(), job).await {
            Ok(()) => {}
            Err(e) => {
                tracing::error!(error = ?e, "discord: process failed");
                let err_msg = format!(":warning: handler error:\n```\n{}\n```", e);
                let _ = msg.channel_id.say(&http, err_msg).await;
            }
        }
    }
}

async fn handle_status() -> Result<String> {
    let resp = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()?
        .get("http://127.0.0.1:8090/api/services")
        .send().await?;
    if !resp.status().is_success() {
        return Ok(format!(":warning: dashboard returned {}", resp.status()));
    }
    let services: Vec<nucleus_core::health::Snapshot> = resp.json().await?;
    let mut out = String::from("**nucleus services:**\n```\n");
    for s in services {
        let icon = match s.status {
            nucleus_core::health::Status::Ok => "✓",
            nucleus_core::health::Status::Degraded => "~",
            nucleus_core::health::Status::Down => "✗",
            nucleus_core::health::Status::Unknown => "?",
        };
        out.push_str(&format!("{} {:20} {}\n", icon, s.id, s.message.unwrap_or_default()));
    }
    out.push_str("```");
    Ok(out)
}

fn cmd_string(cmd: &CommandInteraction, name: &str) -> Option<String> {
    cmd.data.options.iter()
        .find(|o| o.name == name)
        .and_then(|o| match &o.value {
            CommandDataOptionValue::String(s) => Some(s.clone()),
            _ => None,
        })
}

fn cmd_int(cmd: &CommandInteraction, name: &str) -> Option<i64> {
    cmd.data.options.iter()
        .find(|o| o.name == name)
        .and_then(|o| match &o.value {
            CommandDataOptionValue::Integer(i) => Some(*i),
            _ => None,
        })
}

async fn handle_news(cmd: &CommandInteraction) -> Result<String> {
    let limit = cmd_int(cmd, "limit").unwrap_or(5).clamp(1, 20);
    let today = chrono::Utc::now().format("%Y-%m-%d");
    let url = format!("http://127.0.0.1:8080/api/items/notable?fetch_date={}&limit={}", today, limit);
    let items: Vec<serde_json::Value> = reqwest::Client::new().get(&url).send().await?.error_for_status()?.json().await?;
    if items.is_empty() {
        return Ok(format!("no notable items in today's fetch ({})", today));
    }
    let mut out = format!("📰 **Top {} notable today ({}):**\n", items.len(), today);
    for it in &items {
        let title = it.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let url = it.get("url").and_then(|v| v.as_str()).unwrap_or("");
        let score = it.get("notable_score").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let reason = it.get("notable_reason").and_then(|v| v.as_str()).unwrap_or("");
        out.push_str(&format!("• [{:.2}] **{}** — {}\n  {}\n", score, title, reason, url));
    }
    Ok(out)
}

async fn handle_remember(cmd: &CommandInteraction, settings: &Settings) -> Result<String> {
    use nucleus_core::memory::{Kind, Memory, promote};
    let fact = cmd_string(cmd, "fact").ok_or_else(|| anyhow::anyhow!("fact required"))?;
    let kind_str = cmd_string(cmd, "kind").unwrap_or_else(|| "reference".into());
    let kind = match kind_str.as_str() {
        "user" => Kind::User,
        "feedback" => Kind::Feedback,
        "project" => Kind::Project,
        _ => Kind::Reference,
    };
    // Generate a kebab-case slug from the first few words of the fact.
    let slug: String = fact.split_whitespace().take(5).collect::<Vec<_>>().join("-")
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-')
        .collect::<String>()
        .to_lowercase();
    let slug = if slug.is_empty() { format!("note-{}", chrono::Utc::now().timestamp()) } else { slug };
    // Sign with the resolved persona display name (ADR-009), not a hardcoded
    // "Jerry" — this footer is persisted into the Tier-2 memory file.
    let signer = nucleus_core::config::resolve_persona(&settings.identity, "discord", None)
        .map(|p| p.display_name)
        .unwrap_or_else(|_| "nucleus".into());
    let mem = Memory {
        name: slug.clone(),
        description: fact.chars().take(120).collect(),
        kind,
        body: format!("{}\n\n_Saved via /remember by {} at {}._", fact.trim(), signer, chrono::Utc::now().to_rfc3339()),
    };
    let path = promote(&mem)?;
    Ok(format!(":white_check_mark: saved **{}** ({:?}) → `{}`", slug, kind, path.display()))
}

async fn handle_forget(cmd: &CommandInteraction) -> Result<String> {
    let name = cmd_string(cmd, "name").ok_or_else(|| anyhow::anyhow!("name required"))?;
    // forget() removes the file AND its MEMORY.md index line (no dangling link).
    if nucleus_core::memory::forget(&name)? {
        Ok(format!(":wastebasket: removed `{}`", name))
    } else {
        Ok(format!(":question: no memory named `{}`", name))
    }
}

/// Versioned migrations (ADR-020): v1 = the historical ensure_schema
/// body. New schema changes go in as v2+ and run exactly once.
const MIGRATIONS: &[nucleus_core::migrate::Migration] = &[nucleus_core::migrate::Migration {
    version: 1,
    name: "baseline-channel-sessions",
    step: nucleus_core::migrate::Step::Sql(
        r#"
        CREATE TABLE IF NOT EXISTS channel_sessions (
            channel_id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL,
            created_at TEXT NOT NULL,
            last_active TEXT NOT NULL,
            turns INTEGER NOT NULL DEFAULT 0
        )
        "#,
    ),
}];

async fn lookup_session(pool: &SqlitePool, channel_id: u64) -> Result<Option<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT session_id FROM channel_sessions WHERE channel_id = ?1",
    )
    .bind(channel_id.to_string())
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(s,)| s))
}

async fn save_session(pool: &SqlitePool, channel_id: u64, session_id: &str, is_new: bool) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    if is_new {
        sqlx::query(
            r#"INSERT OR REPLACE INTO channel_sessions
               (channel_id, session_id, created_at, last_active, turns)
               VALUES (?1, ?2, ?3, ?3, 1)"#,
        )
        .bind(channel_id.to_string())
        .bind(session_id)
        .bind(&now)
        .execute(pool)
        .await?;
    } else {
        sqlx::query(
            r#"UPDATE channel_sessions
               SET session_id = ?2, last_active = ?3, turns = turns + 1
               WHERE channel_id = ?1"#,
        )
        .bind(channel_id.to_string())
        .bind(session_id)
        .bind(&now)
        .execute(pool)
        .await?;
    }
    Ok(())
}

async fn send_chunked(http: &Http, channel: ChannelId, body: &str) -> Result<()> {
    // Suppress URL link previews on every outbound message — they're chat
    // replies, not curated link cards, and embeds 5x the visual size for
    // no benefit.
    let new_msg = |slice: &str| CreateMessage::new()
        .content(slice.to_string())
        .flags(MessageFlags::SUPPRESS_EMBEDS);

    if body.len() <= MAX_DISCORD_CHARS {
        channel.send_message(http, new_msg(body)).await.context("discord send_message")?;
        return Ok(());
    }
    let mut start = 0;
    while start < body.len() {
        let mut end = (start + MAX_DISCORD_CHARS).min(body.len());
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        if end < body.len() {
            if let Some(nl) = body[start..end].rfind('\n') {
                end = start + nl + 1;
            }
        }
        channel.send_message(http, new_msg(&body[start..end])).await.context("discord send_message chunked")?;
        start = end;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    nucleus_core::init_tracing();
    let settings = Settings::load().context("loading nucleus.toml + .env")?;
    let token = std::env::var("DISCORD_BOT_TOKEN").context("DISCORD_BOT_TOKEN not set")?;

    let workspace_root = std::env::current_dir()?;
    let persona = nucleus_core::config::resolve_persona(&settings.identity, "discord", None)
        .context("resolving Discord persona (ADR-009)")?;

    let db_path = workspace_root.join(DB_PATH);
    let pool = db::open(&db_path).await.context("opening discord.db")?;
    nucleus_core::migrate::migrate(&pool, MIGRATIONS)
        .await
        .context("migrating discord.db")?;

    // Tear down any leftover tmux session from a previous crash before we
    // create fresh windows for live conversations.
    let _ = tokio::process::Command::new("tmux")
        .args(["kill-session", "-t", "nucleus-discord"])
        .output()
        .await;

    let (pool_config, ask_options) = session_profile::interactive_pool(
        &ProfileContext {
            workspace_root: &workspace_root,
            claude: &settings.claude,
            tmux_session: "nucleus-discord",
            agent_label: "discord",
        },
        persona.body,
        std::time::Duration::from_secs(60 * 60 * 4),
        if settings.skill_learner.enabled {
            settings.skill_learner.nudge_interval
        } else {
            0
        },
    );
    let sessions = Arc::new(SessionPool::new(pool_config));

    // Background task: reap idle sessions every 30 minutes.
    {
        let sessions = sessions.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60 * 30));
            tick.tick().await; // skip the immediate first tick
            loop {
                tick.tick().await;
                if let Ok(n) = sessions.reap_idle().await {
                    if n > 0 {
                        tracing::info!("discord: reaped {} idle sessions", n);
                    }
                }
            }
        });
    }

    // Background task: daily 04:00 local rotation. Summarize each active
    // chat session, write the summary to today's diary, spawn a fresh
    // session primed with the summary + last 10 turns, persist the new
    // session-id back to channel_sessions. Keeps user-facing ask() calls
    // from ever hitting the in-line "Resume from summary?" compaction.
    {
        let sessions = sessions.clone();
        let db_pool = pool.clone();
        tokio::spawn(async move {
            loop {
                nucleus_core::claude_session::sleep_until_next_4am().await;
                let pool_for_cb = db_pool.clone();
                let stats = sessions
                    .daily_rotate("discord", move |chat_key, new_session_id| {
                        let pool_for_cb = pool_for_cb.clone();
                        async move {
                            let channel_id: u64 = chat_key
                                .parse()
                                .context("rotation callback: parse channel_id")?;
                            save_session(&pool_for_cb, channel_id, &new_session_id, true).await
                        }
                    })
                    .await;
                tracing::info!(
                    "discord: daily rotation done — considered={} rotated={} skipped={} failed={}",
                    stats.considered, stats.rotated, stats.skipped, stats.failed
                );
            }
        });
    }

    let handler = Handler {
        pool,
        sessions,
        ask_options,
        settings: settings.clone(),
        workspace_root,
        bot_user_id: tokio::sync::OnceCell::new(),
        home_channel_id: settings.discord.home_channel_id.parse().unwrap_or_default(),
    };

    let intents = GatewayIntents::GUILDS
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT;

    let mut client = Client::builder(&token, intents)
        .event_handler(handler)
        .await
        .context("building serenity client")?;

    tracing::info!("discord: starting up");
    client.start().await.context("running serenity client")?;
    Ok(())
}
