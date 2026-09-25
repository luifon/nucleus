//! Outbound Discord helpers — direct REST, no serenity dependency.
//! Used by binaries that need to fan out one-shot notifications (news fetcher,
//! reminders). The Discord bot uses serenity directly for its own
//! inbound + outbound flow.

use anyhow::{Context, Result};
use serde_json::json;

const API_BASE: &str = "https://discord.com/api/v10";

fn token() -> Result<String> {
    std::env::var("DISCORD_BOT_TOKEN").map_err(|_| NotSent("DISCORD_BOT_TOKEN env var not set".into()).into())
}

/// A send that provably posted nothing: no token, no connection to Discord,
/// or a 4xx answer (Discord creates a message only with a 2xx answer, and a
/// 4xx rejects the request). A timeout, a connection lost after the request
/// left, or a 5xx answer is a different error: the message may exist.
#[derive(Debug)]
pub struct NotSent(pub String);

impl std::fmt::Display for NotSent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotSent {}

/// The error proves the message was not posted.
pub fn not_sent(e: &anyhow::Error) -> bool {
    e.downcast_ref::<NotSent>().is_some()
}

/// Discord message flags. Bitfield — combine with `|`.
pub mod flags {
    pub const SUPPRESS_EMBEDS: u32 = 4;
    pub const SUPPRESS_NOTIFICATIONS: u32 = 1024;
}

/// Send a single message to a Discord channel via REST. Returns the new message ID.
/// `suppress_embeds=true` disables URL link previews — use for compact bulleted lists.
pub async fn send_message(channel_id: &str, content: &str, suppress_embeds: bool) -> Result<String> {
    // parse: [] so a generic send can never ping @everyone/@here/roles from
    // content. send_announcement is the explicit opt-in for broadcasts.
    let mut body = json!({ "content": content, "allowed_mentions": { "parse": [] } });
    if suppress_embeds {
        body["flags"] = json!(flags::SUPPRESS_EMBEDS);
    }
    post_message(channel_id, body).await
}

/// [`send_message`] with a Discord `nonce` and `enforce_nonce`: a second
/// request with the same nonce within a few minutes returns the message the
/// first one created instead of posting again. `nonce` is at most 25
/// characters. Used by retried deliveries (task results).
pub async fn send_message_once(
    channel_id: &str,
    content: &str,
    suppress_embeds: bool,
    nonce: &str,
) -> Result<String> {
    let mut body = json!({
        "content": content,
        "allowed_mentions": { "parse": [] },
        "nonce": nonce,
        "enforce_nonce": true,
    });
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
        .map_err(|e| {
            if e.is_connect() || e.is_builder() {
                anyhow::Error::new(NotSent(format!("posting message to discord: {e}")))
            } else {
                anyhow::Error::new(e).context("posting message to discord")
            }
        })?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.is_client_error() {
        return Err(NotSent(format!("discord POST {url} failed: {status} — {text}")).into());
    }
    if !status.is_success() {
        anyhow::bail!("discord POST {} failed: {} — {}", url, status, text);
    }
    let parsed: serde_json::Value =
        serde_json::from_str(&text).context("parsing discord response")?;
    Ok(parsed.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string())
}
