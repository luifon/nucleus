//! The widget seam (ADR-031). Two files in one directory, each owned by
//! exactly one side:
//!
//! - `news.json` — written here, read by the widget. Replaced atomically so
//!   the widget never observes a half-written day.
//! - `news-votes.json` — written by the widget, read here. The fetcher never
//!   deletes or rewrites it; the widget prunes its own outbox.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::path::Path;

pub const FEED_FILE: &str = "news.json";
pub const VOTES_FILE: &str = "news-votes.json";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Feed {
    /// Local time of the run, with offset.
    pub as_of: String,
    pub brief: String,
    pub count: usize,
    pub items: Vec<FeedItem>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedItem {
    pub id: String,
    pub title: String,
    pub source: String,
    pub url: String,
    pub published_at: String,
    pub score: f64,
    pub reason: String,
    pub event: String,
    /// Effective vote held for this item: 1, -1 or 0.
    pub vote: i64,
}

/// Write `news.json` into `dir`, creating the directory if it doesn't exist.
/// Temp file plus rename, on the same filesystem, so a reader either sees the
/// previous day or the complete new one.
pub fn write_feed(dir: &Path, feed: &Feed) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating the widget feed dir {}", dir.display()))?;
    let final_path = dir.join(FEED_FILE);
    let tmp_path = dir.join(format!("{FEED_FILE}.tmp"));
    let body = serde_json::to_vec_pretty(feed)?;
    std::fs::write(&tmp_path, &body)
        .with_context(|| format!("writing {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, &final_path)
        .with_context(|| format!("replacing {}", final_path.display()))?;
    Ok(())
}

#[derive(Debug, Deserialize)]
struct VoteOutbox {
    #[serde(default)]
    votes: Vec<OutboxVote>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OutboxVote {
    vote_id: String,
    item_id: String,
    vote: i64,
    at: String,
}

/// Drain the widget's vote outbox into the DB. Idempotent by `voteId`, so
/// replaying the whole file every run is the normal case, not a fallback.
/// Returns how many rows were new.
///
/// A missing or unparseable file is not an error — the widget may simply
/// never have written one.
pub async fn ingest_votes(pool: &SqlitePool, dir: &Path) -> Result<usize> {
    let path = dir.join(VOTES_FILE);
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "no widget vote outbox to read");
            return Ok(0);
        }
    };
    let outbox: VoteOutbox = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "widget vote outbox did not parse");
            return Ok(0);
        }
    };

    let mut new = 0usize;
    for v in &outbox.votes {
        if !(-1..=1).contains(&v.vote) {
            tracing::debug!(vote_id = %v.vote_id, vote = v.vote, "skipping out-of-range widget vote");
            continue;
        }
        if crate::store::insert_vote(pool, &v.vote_id, &v.item_id, v.vote, "widget", &v.at).await? {
            new += 1;
        }
    }
    if new > 0 {
        tracing::info!(new, total = outbox.votes.len(), "ingested widget votes");
    }
    Ok(new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_serializes_to_the_widget_contract() {
        let feed = Feed {
            as_of: "2026-09-13T09:00:00-03:00".into(),
            brief: "b".into(),
            count: 1,
            items: vec![FeedItem {
                id: "abc".into(),
                title: "t".into(),
                source: "Hacker News".into(),
                url: "https://example.com/x".into(),
                published_at: "2026-09-13T08:00:00+00:00".into(),
                score: 0.91,
                reason: "r".into(),
                event: "e".into(),
                vote: 0,
            }],
        };
        let v: serde_json::Value = serde_json::to_value(&feed).unwrap();
        assert!(v.get("asOf").is_some(), "asOf must be camelCase");
        assert_eq!(v["items"][0]["publishedAt"], "2026-09-13T08:00:00+00:00");
        assert_eq!(v["items"][0]["score"], 0.91);
    }

    #[test]
    fn outbox_parses_camel_case_and_tolerates_an_empty_list() {
        let o: VoteOutbox = serde_json::from_str(
            r#"{"votes":[{"voteId":"v1","itemId":"i1","vote":-1,"at":"2026-09-13T09:00:00Z"}]}"#,
        )
        .unwrap();
        assert_eq!(o.votes[0].vote_id, "v1");
        assert_eq!(o.votes[0].vote, -1);
        let empty: VoteOutbox = serde_json::from_str("{}").unwrap();
        assert!(empty.votes.is_empty());
    }
}
