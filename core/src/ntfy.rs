//! Push notifications through an ntfy server (ADR-037).
//!
//! Configuration comes from `.env`: `NUCLEUS_NTFY_URL` (the server, for
//! example `https://ntfy.example.com`), `NUCLEUS_NTFY_TOPIC` and
//! `NUCLEUS_NTFY_TOKEN`, an access token of an ntfy user that may only
//! publish to that topic. The channel is off unless all three are set.
//!
//! Every message passes the credential filter ([`crate::secret_filter`])
//! before it leaves, as other text Rust sends out does.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::Path;

use crate::secret_filter::CredentialRules;

/// ntfy's default `message-size-limit` is 4096 bytes; a longer message
/// becomes an attachment instead of a notification body.
const MAX_MESSAGE_BYTES: usize = 4000;

/// Where to publish. Built from the environment by [`NtfyConfig::from_env`].
#[derive(Clone)]
pub struct NtfyConfig {
    /// Server base URL without a trailing slash.
    pub url: String,
    pub topic: String,
    pub token: String,
}

impl std::fmt::Debug for NtfyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NtfyConfig")
            .field("url", &self.url)
            .field("topic", &self.topic)
            .field("token", &"[redacted]")
            .finish()
    }
}

impl NtfyConfig {
    /// The configuration from `NUCLEUS_NTFY_URL`, `NUCLEUS_NTFY_TOPIC` and
    /// `NUCLEUS_NTFY_TOKEN`, or `None` when any of them is missing or empty.
    pub fn from_env() -> Option<Self> {
        Self::from_values(
            std::env::var("NUCLEUS_NTFY_URL").ok(),
            std::env::var("NUCLEUS_NTFY_TOPIC").ok(),
            std::env::var("NUCLEUS_NTFY_TOKEN").ok(),
        )
    }

    fn from_values(url: Option<String>, topic: Option<String>, token: Option<String>) -> Option<Self> {
        let clean = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
        let url = clean(url)?;
        let (topic, token) = (clean(topic)?, clean(token)?);
        if let Err(why) = server_root(&url) {
            tracing::warn!("NUCLEUS_NTFY_URL ignored, the ntfy channel is off: {why}");
            return None;
        }
        Some(Self { url: url.trim_end_matches('/').to_string(), topic, token })
    }
}

/// `url` must name the server root. JSON publishing only happens at `/`; at a
/// topic path such as `https://host/alerts`, ntfy publishes the JSON text as
/// the plain message and still answers with success.
fn server_root(url: &str) -> std::result::Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("not a URL ({e})"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(format!("scheme must be http or https, not {:?}", parsed.scheme()));
    }
    if parsed.path() != "/" || parsed.query().is_some() || parsed.fragment().is_some() {
        return Err("must be the server root, with no path, query or fragment".into());
    }
    Ok(())
}

/// ntfy message priority, 1 (min) to 5 (max). `High` and `Max` sound through
/// the phone's notification settings for the topic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    Min = 1,
    Low = 2,
    Default = 3,
    High = 4,
    Max = 5,
}

/// One notification.
#[derive(Debug, Default)]
pub struct Notification<'a> {
    pub title: Option<&'a str>,
    pub message: &'a str,
    pub priority: Option<Priority>,
    /// ntfy tags; names of emoji shortcodes (for example `bell`) show as an
    /// icon before the title.
    pub tags: &'a [&'a str],
    /// A URL opened when the notification is tapped.
    pub click: Option<&'a str>,
}

#[derive(Serialize)]
struct PublishBody<'a> {
    topic: &'a str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    priority: Option<u8>,
    #[serde(skip_serializing_if = "<[&str]>::is_empty")]
    tags: &'a [&'a str],
    #[serde(skip_serializing_if = "Option::is_none")]
    click: Option<&'a str>,
}

/// The request body for `n`: credentials removed from the title and the
/// message, the message capped at [`MAX_MESSAGE_BYTES`].
fn body<'a>(cfg: &'a NtfyConfig, n: &'a Notification<'a>, rules: &CredentialRules) -> PublishBody<'a> {
    let message = cap_bytes(&rules.redact(n.message).text, MAX_MESSAGE_BYTES);
    PublishBody {
        topic: &cfg.topic,
        message,
        title: n.title.map(|t| rules.redact(t).text),
        priority: n.priority.map(|p| p as u8),
        tags: n.tags,
        click: n.click,
    }
}

/// `s` cut to at most `max` bytes at a character boundary, with `…` when cut
/// and `max` leaves room for it.
fn cap_bytes(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let ellipsis = '…'.len_utf8();
    let room = if max >= ellipsis { max - ellipsis } else { max };
    let mut end = room;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    if max >= ellipsis { format!("{}…", &s[..end]) } else { s[..end].to_string() }
}

/// The credential rules for one publish: the workspace `.env` values plus the
/// configured token itself, which may come from the process environment
/// rather than `.env`.
fn rules(cfg: &NtfyConfig, env_text: &str) -> CredentialRules {
    CredentialRules::from_env_text(&format!("{env_text}\nNUCLEUS_NTFY_TOKEN={}\n", cfg.token))
}

/// Server error text safe to log and store: credentials and the token removed,
/// capped at 300 bytes.
fn error_detail(cfg: &NtfyConfig, rules: &CredentialRules, text: &str) -> String {
    let text = text.trim().replace(&cfg.token, "[redacted]");
    cap_bytes(&rules.redact(&text).text, 300)
}

/// Publishes `n` and returns the ntfy message ID.
pub async fn publish(cfg: &NtfyConfig, workspace_root: &Path, n: &Notification<'_>) -> Result<String> {
    let env_text = std::fs::read_to_string(workspace_root.join(".env")).unwrap_or_default();
    let rules = rules(cfg, &env_text);
    let body = body(cfg, n, &rules);
    // JSON publishing goes to the server root; the topic is in the body.
    let resp = reqwest::Client::new()
        .post(&cfg.url)
        .bearer_auth(&cfg.token)
        .timeout(std::time::Duration::from_secs(20))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("publishing to ntfy at {}", cfg.url))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!(
            "ntfy publish to {}/{} failed: {} — {}",
            cfg.url,
            cfg.topic,
            status,
            error_detail(cfg, &rules, &text)
        );
    }
    let parsed: serde_json::Value = serde_json::from_str(&text).context("parsing ntfy response")?;
    Ok(parsed.get("id").and_then(|v| v.as_str()).unwrap_or_default().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> NtfyConfig {
        NtfyConfig { url: "https://ntfy.test".into(), topic: "nucleus".into(), token: "tk_x".into() }
    }

    #[test]
    fn config_needs_all_three_values() {
        let some = |s: &str| Some(s.to_string());
        let full = NtfyConfig::from_values(some("https://ntfy.test/"), some("nucleus"), some("tk_a")).unwrap();
        assert_eq!(full.url, "https://ntfy.test", "trailing slash removed");
        assert!(NtfyConfig::from_values(None, some("nucleus"), some("tk_a")).is_none());
        assert!(NtfyConfig::from_values(some("https://ntfy.test"), some("  "), some("tk_a")).is_none());
        assert!(NtfyConfig::from_values(some("https://ntfy.test"), some("nucleus"), None).is_none());
    }

    #[test]
    fn debug_output_hides_the_token() {
        let shown = format!("{:?}", cfg());
        assert!(!shown.contains("tk_x"), "{shown}");
    }

    #[test]
    fn body_redacts_credentials_and_keeps_fields() {
        let rules = CredentialRules::from_env_text("NUCLEUS_NTFY_TOKEN=tk_secretvalue123\n");
        let n = Notification {
            title: Some("Reminder"),
            message: "the token is tk_secretvalue123, done",
            priority: Some(Priority::High),
            tags: &["bell"],
            click: None,
        };
        let c = cfg();
        let json = serde_json::to_value(body(&c, &n, &rules)).unwrap();
        assert_eq!(json["topic"], "nucleus");
        assert_eq!(json["title"], "Reminder");
        assert_eq!(json["priority"], 4);
        assert_eq!(json["tags"][0], "bell");
        assert!(json.get("click").is_none(), "unset fields are omitted: {json}");
        let message = json["message"].as_str().unwrap();
        assert!(!message.contains("tk_secretvalue123"), "{message}");
    }

    #[test]
    fn long_messages_are_capped_at_a_char_boundary() {
        let long = "é".repeat(3000); // 6000 bytes
        let capped = cap_bytes(&long, MAX_MESSAGE_BYTES);
        assert!(capped.len() <= MAX_MESSAGE_BYTES);
        assert!(capped.ends_with('…'));
        assert_eq!(cap_bytes("short", MAX_MESSAGE_BYTES), "short");
        // A limit smaller than the ellipsis cuts without one instead of panicking.
        assert_eq!(cap_bytes("abcdef", 2), "ab");
    }

    #[test]
    fn url_must_be_the_server_root() {
        let some = |s: &str| Some(s.to_string());
        let with = |u: &str| NtfyConfig::from_values(some(u), some("topic-x"), some("tk_a"));
        assert!(with("https://ntfy.test").is_some());
        assert!(with("https://ntfy.test/").is_some());
        assert!(with("https://ntfy.test/alerts").is_none(), "a topic path publishes JSON as text");
        assert!(with("https://ntfy.test/?x=1").is_none());
        assert!(with("ftp://ntfy.test").is_none());
        assert!(with("not a url").is_none());
    }

    #[test]
    fn the_configured_token_is_redacted_even_when_not_in_env() {
        let c = NtfyConfig { token: "tk_onlyinprocessenv42".into(), ..cfg() };
        let rules = rules(&c, "OTHER_SETTING=1\n");
        let n = Notification { message: "leak tk_onlyinprocessenv42 here", ..Default::default() };
        let json = serde_json::to_value(body(&c, &n, &rules)).unwrap();
        assert!(!json["message"].as_str().unwrap().contains("tk_onlyinprocessenv42"), "{json}");
    }

    #[test]
    fn error_detail_hides_the_token_and_is_capped() {
        let c = NtfyConfig { token: "tk_echoedback99".into(), ..cfg() };
        let rules = rules(&c, "");
        let echoed = format!("unauthorized: Bearer tk_echoedback99 {}", "x".repeat(1000));
        let detail = error_detail(&c, &rules, &echoed);
        assert!(!detail.contains("tk_echoedback99"), "{detail}");
        assert!(detail.len() <= 300);
    }
}
