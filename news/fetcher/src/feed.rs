//! Feed fetching and parsing. Network in, `ParsedItem`s out — no scoring,
//! no database, no policy.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::canonical::canonicalize;

#[derive(Debug, Clone)]
pub struct SourceRow {
    pub id: i64,
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone)]
pub struct ParsedItem {
    /// sha256 of the canonical URL, truncated. Stable across runs and across
    /// sources, so the widget can vote on an id that outlives a re-fetch.
    pub id: String,
    pub source_id: i64,
    pub source_name: String,
    /// The URL clicks land on. For aggregators this is the discussion page.
    pub url: String,
    /// The underlying primary-source URL when `url` is a discussion page.
    pub article_url: Option<String>,
    /// `COALESCE(article_url, url)` canonicalized — the item's identity.
    pub canonical_url: String,
    pub title: String,
    pub summary: Option<String>,
    pub published: DateTime<Utc>,
    pub published_at: String,
    pub published_date: String,
}

impl ParsedItem {
    /// Host of the canonical URL, for the ranker's quality/staleness judgement.
    pub fn domain(&self) -> String {
        url::Url::parse(&self.canonical_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_string()))
            .unwrap_or_default()
    }
}

/// Browser-like UA: Reddit-lineage endpoints (lobste.rs, some GitHub feeds)
/// reject the default reqwest agent and anything ending in `/<version>`.
pub const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) AppleWebKit/537.36 (KHTML, like Gecko) nucleus-news-fetcher";

pub fn http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(std::time::Duration::from_secs(20))
        .build()?)
}

pub async fn fetch_source(http: &reqwest::Client, src: &SourceRow) -> Result<Vec<ParsedItem>> {
    let bytes = http.get(&src.url).send().await?.error_for_status()?.bytes().await?;
    let feed = feed_rs::parser::parse(&bytes[..]).context("parsing feed")?;
    let mut out = Vec::with_capacity(feed.entries.len());
    for entry in feed.entries {
        let feed_link = entry.links.first().map(|l| l.href.clone()).unwrap_or_default();
        if feed_link.is_empty() {
            continue;
        }
        let title = entry.title.map(|t| t.content).unwrap_or_else(|| "(untitled)".into());
        let raw_summary = entry.summary.map(|s| s.content);
        let summary = raw_summary.as_deref().map(sanitize_summary);

        let (url, article_url) = pick_primary_url(&src.name, &feed_link, raw_summary.as_deref());
        let canonical_url = canonicalize(article_url.as_deref().unwrap_or(&url));
        let published = entry.published.or(entry.updated).unwrap_or_else(Utc::now);

        out.push(ParsedItem {
            id: item_id(&canonical_url),
            source_id: src.id,
            source_name: src.name.clone(),
            url,
            article_url,
            canonical_url,
            title,
            summary,
            published,
            published_at: published.to_rfc3339(),
            published_date: published.format("%Y-%m-%d").to_string(),
        });
    }
    Ok(out)
}

/// Identity is the canonical destination, not the source that carried it.
pub fn item_id(canonical_url: &str) -> String {
    let mut h = Sha256::new();
    h.update(canonical_url.as_bytes());
    hex::encode(&h.finalize()[..12])
}

/// Pick the URL clicks should land on (`url`) and the underlying article URL
/// (`article_url`).
///
/// - **Hacker News**: hnrss.org puts the comments link inline in the body as
///   "Comments URL: https://news.ycombinator.com/item?id=…", and the feed's
///   own `<link>` is the *article*. We swap them.
/// - **lobste.rs**: same shape, labelled "Comments:".
/// - **Everything else**: the feed link is the article; no discussion page.
pub fn pick_primary_url(
    source_name: &str,
    feed_link: &str,
    raw_summary: Option<&str>,
) -> (String, Option<String>) {
    let body = raw_summary.unwrap_or("");

    let comments_url = match source_name {
        "Hacker News" => extract_url_after_label(body, "comments url:"),
        "lobste.rs" => extract_url_after_label(body, "comments:")
            .or_else(|| extract_url_after_label(body, "comments url:")),
        _ => None,
    };

    match comments_url {
        Some(disc) if !disc.is_empty() && disc != feed_link => (disc, Some(feed_link.to_string())),
        _ => (feed_link.to_string(), None),
    }
}

/// First `http(s)://…` URL appearing after a case-insensitive label. Works on
/// raw HTML and on sanitized one-line summaries alike — it depends on neither
/// line breaks nor tags. Trailing punctuation is stripped.
fn extract_url_after_label(body: &str, needle_lc: &str) -> Option<String> {
    let lower = body.to_lowercase();
    let label_pos = lower.find(needle_lc)?;
    let after = &body[label_pos + needle_lc.len()..];
    let url_start = after.find("http")?;
    let url_part = &after[url_start..];
    let url_end = url_part
        .find(|c: char| c.is_whitespace() || matches!(c, '"' | '<' | '>' | '\'' | '`'))
        .unwrap_or(url_part.len());
    let raw = &url_part[..url_end];
    let trimmed = raw.trim_end_matches([',', '.', ')', ']', '}', ';', ':']);
    if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
}

/// Strip tags, collapse whitespace, cap length. Truncation counts characters,
/// not bytes — a byte-index cut lands mid-codepoint on smart quotes and emoji
/// and panics.
pub fn sanitize_summary(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match (in_tag, ch) {
            (false, '<') => in_tag = true,
            (true, '>') => in_tag = false,
            (false, c) => out.push(c),
            _ => {}
        }
    }
    let trimmed: String = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.chars().count() > 600 {
        let head: String = trimmed.chars().take(600).collect();
        format!("{head}...")
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hn_swaps_discussion_and_article() {
        let body = "<p>Article URL: <a href=\"https://ex.com/a\">https://ex.com/a</a></p>\
                    <p>Comments URL: <a href=\"https://news.ycombinator.com/item?id=1\">x</a></p>";
        let (url, article) = pick_primary_url("Hacker News", "https://ex.com/a", Some(body));
        assert_eq!(url, "https://news.ycombinator.com/item?id=1");
        assert_eq!(article.as_deref(), Some("https://ex.com/a"));
    }

    #[test]
    fn plain_source_keeps_the_feed_link() {
        let (url, article) = pick_primary_url("Julia Evans", "https://jvns.ca/x", Some("body"));
        assert_eq!(url, "https://jvns.ca/x");
        assert!(article.is_none());
    }

    #[test]
    fn identity_ignores_the_carrying_source() {
        // Same article via two aggregators → one id.
        assert_eq!(
            item_id(&canonicalize("https://ex.com/post?utm_source=hn")),
            item_id(&canonicalize("https://www.ex.com/post/"))
        );
    }

    #[test]
    fn summary_truncation_is_codepoint_safe() {
        let s = "é".repeat(1000);
        let out = sanitize_summary(&s);
        assert_eq!(out.chars().count(), 603); // 600 + "..."
    }
}
