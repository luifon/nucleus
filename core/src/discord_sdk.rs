//! Outbound Discord helpers — direct REST, no serenity dependency.
//! Used by binaries that need to fan out one-shot notifications (news fetcher,
//! reminders). The Discord bot (`jerry`) uses serenity directly for its own
//! inbound + outbound flow.

use anyhow::{Context, Result};
use serde_json::json;

const API_BASE: &str = "https://discord.com/api/v10";

fn token() -> Result<String> {
    std::env::var("DISCORD_BOT_TOKEN").context("DISCORD_BOT_TOKEN env var not set")
}

/// Discord message flags. Bitfield — combine with `|`.
pub mod flags {
    pub const SUPPRESS_EMBEDS: u32 = 4;
    pub const SUPPRESS_NOTIFICATIONS: u32 = 1024;
}

/// Send a single message to a Discord channel via REST. Returns the new message ID.
/// `suppress_embeds=true` disables URL link previews — use for compact bulleted lists.
pub async fn send_message(channel_id: &str, content: &str, suppress_embeds: bool) -> Result<String> {
    let mut body = json!({ "content": content });
    if suppress_embeds {
        body["flags"] = json!(flags::SUPPRESS_EMBEDS);
    }
    post_message(channel_id, body).await
}

/// Send a channel announcement — suppresses URL embeds AND enables `@here` / `@everyone`
/// / user / role parsing. Default `send_message` strips those for safety; this is the
/// explicit opt-in for things like the daily news post.
pub async fn send_announcement(channel_id: &str, content: &str) -> Result<String> {
    let body = json!({
        "content": content,
        "flags": flags::SUPPRESS_EMBEDS,
        "allowed_mentions": {
            "parse": ["everyone", "users", "roles"]
        }
    });
    post_message(channel_id, body).await
}

async fn post_message(channel_id: &str, body: serde_json::Value) -> Result<String> {
    let token = token()?;
    let url = format!("{}/channels/{}/messages", API_BASE, channel_id);
    let resp = reqwest::Client::new()
        .post(&url)
        .header("Authorization", format!("Bot {}", token))
        .json(&body)
        .send()
        .await
        .context("posting message to discord")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("discord POST {} failed: {} — {}", url, status, text);
    }
    let parsed: serde_json::Value =
        serde_json::from_str(&text).context("parsing discord response")?;
    Ok(parsed.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string())
}
