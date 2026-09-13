//! The widget seam (ADR-031). Three files in one directory, each owned by
//! exactly one side:
//!
//! - `news.json` — written here, read by the widget. Replaced atomically so
//!   the widget never observes a half-written day.
//! - `news-votes.json` — written by the widget, read here. The fetcher never
//!   deletes or rewrites it; the widget prunes its own outbox.
//! - `news-opens.json` — same ownership, for click-throughs.
//!
//! Both outboxes are drained with the same discipline: append-only on the
//! widget's side, idempotent by id on ours, replayed whole every run. A
//! missing file is the normal state of a widget that hasn't been used yet, not
//! an error.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::path::Path;

use crate::store;

pub const FEED_FILE: &str = "news.json";
pub const VOTES_FILE: &str = "news-votes.json";
pub const OPENS_FILE: &str = "news-opens.json";

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
    /// Why the effective vote was cast, when the reader said. One of
    /// `store::VOTE_REASONS`; null for a vote cast without one, and for
    /// every upvote.
    pub vote_reason: Option<String>,
    /// Free text attached to the effective vote.
    pub vote_note: Option<String>,
    /// Whether the reader has ever clicked through to this item. Round-trips
    /// so the widget can mark it read; it changes nothing about the ranking.
    pub opened: bool,
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
    /// Why, when the reader picked a reason. A reason arrives as a whole new
    /// vote entry rather than as an edit of the one it explains — the outbox
    /// is append-only on both sides of the seam.
    #[serde(default)]
    vote_reason: Option<String>,
    #[serde(default)]
    vote_note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenOutbox {
    #[serde(default)]
    opens: Vec<OutboxOpen>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OutboxOpen {
    open_id: String,
    item_id: String,
    url: String,
    at: String,
}

/// Drain both widget outboxes into the DB. Returns how many rows were new,
/// across the two.
pub async fn ingest_outboxes(pool: &SqlitePool, dir: &Path) -> Result<usize> {
    Ok(ingest_votes(pool, dir).await? + ingest_opens(pool, dir).await?)
}

/// Read and parse one outbox file. A missing or unparseable file yields None:
/// the widget may simply never have written one, and a half-written file is
/// the next run's problem, not this run's failure.
fn read_outbox<T: for<'de> Deserialize<'de>>(path: &Path, what: &str) -> Option<T> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(path = %path.display(), error = %e, "no widget {what} outbox to read");
            return None;
        }
    };
    match serde_json::from_str(&raw) {
        Ok(v) => Some(v),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "widget {what} outbox did not parse");
            None
        }
    }
}

/// Drain the widget's vote outbox into the DB. Idempotent by `voteId`, so
/// replaying the whole file every run is the normal case, not a fallback.
/// Returns how many rows were new.
///
/// Order matters here and nowhere else in the file: rows go in as they appear
/// in the array, so when two votes for one item carry the same timestamp the
/// later entry is the one the effective-vote query picks.
pub async fn ingest_votes(pool: &SqlitePool, dir: &Path) -> Result<usize> {
    let path = dir.join(VOTES_FILE);
    let Some(outbox) = read_outbox::<VoteOutbox>(&path, "vote") else {
        return Ok(0);
    };

    let mut new = 0usize;
    for v in &outbox.votes {
        if !(-1..=1).contains(&v.vote) {
            tracing::debug!(vote_id = %v.vote_id, vote = v.vote, "skipping out-of-range widget vote");
            continue;
        }
        let reason = v.vote_reason.as_deref().map(str::trim).filter(|r| !r.is_empty());
        let reason = match reason {
            Some(r) if store::is_known_vote_reason(r) => Some(r),
            Some(r) => {
                // Version skew: a widget offering a reason this build doesn't
                // know. Keep the vote, drop the label — a key no consumer can
                // read is worse stored than absent.
                tracing::warn!(vote_id = %v.vote_id, reason = %r, "unknown vote reason — storing NULL");
                None
            }
            None => None,
        };
        let note = v.vote_note.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(|n| {
            if n.chars().count() > store::MAX_VOTE_NOTE_CHARS {
                tracing::warn!(vote_id = %v.vote_id, "vote note over the length cap — truncating");
                n.chars().take(store::MAX_VOTE_NOTE_CHARS).collect::<String>()
            } else {
                n.to_string()
            }
        });
        let incoming = store::IncomingVote {
            vote_id: &v.vote_id,
            item_id: &v.item_id,
            vote: v.vote,
            origin: "widget",
            created_at: &v.at,
            reason_key: reason,
            note: note.as_deref(),
        };
        if store::insert_vote(pool, &incoming).await? {
            new += 1;
        }
    }
    if new > 0 {
        tracing::info!(new, total = outbox.votes.len(), "ingested widget votes");
    }
    Ok(new)
}

/// Drain the widget's click-through outbox. Idempotent by `openId`.
///
/// An open is attention, not preference. Nothing downstream scores or ranks on
/// it — it round-trips to the widget as `opened` and it is evidence for the
/// monthly review.
pub async fn ingest_opens(pool: &SqlitePool, dir: &Path) -> Result<usize> {
    let path = dir.join(OPENS_FILE);
    let Some(outbox) = read_outbox::<OpenOutbox>(&path, "open") else {
        return Ok(0);
    };

    let mut new = 0usize;
    for o in &outbox.opens {
        if o.open_id.trim().is_empty() || o.item_id.trim().is_empty() {
            tracing::debug!("skipping widget open with no id");
            continue;
        }
        if store::insert_open(pool, &o.open_id, &o.item_id, &o.url, "widget", &o.at).await? {
            new += 1;
        }
    }
    if new > 0 {
        tracing::info!(new, total = outbox.opens.len(), "ingested widget opens");
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
                vote: -1,
                vote_reason: Some("dup".into()),
                vote_note: None,
                opened: true,
            }],
        };
        let v: serde_json::Value = serde_json::to_value(&feed).unwrap();
        assert!(v.get("asOf").is_some(), "asOf must be camelCase");
        assert_eq!(v["items"][0]["publishedAt"], "2026-09-13T08:00:00+00:00");
        assert_eq!(v["items"][0]["score"], 0.91);
        assert_eq!(v["items"][0]["voteReason"], "dup");
        assert!(v["items"][0]["voteNote"].is_null(), "a missing note is null, not absent");
        assert_eq!(v["items"][0]["opened"], true);
    }

    #[test]
    fn vote_outbox_parses_camel_case_and_tolerates_an_empty_list() {
        let o: VoteOutbox = serde_json::from_str(
            r#"{"votes":[{"voteId":"v1","itemId":"i1","vote":-1,"at":"2026-09-13T09:00:00Z"}]}"#,
        )
        .unwrap();
        assert_eq!(o.votes[0].vote_id, "v1");
        assert_eq!(o.votes[0].vote, -1);
        assert!(o.votes[0].vote_reason.is_none(), "reasons are optional");
        let empty: VoteOutbox = serde_json::from_str("{}").unwrap();
        assert!(empty.votes.is_empty());
    }

    #[test]
    fn vote_outbox_reads_reasons_notes_and_fractional_timestamps() {
        let o: VoteOutbox = serde_json::from_str(
            r#"{"votes":[{"voteId":"v2","itemId":"i1","vote":-1,
                          "at":"2026-09-13T11:26:10.482-03:00",
                          "voteReason":"dup","voteNote":"same story as the Willison post"}]}"#,
        )
        .unwrap();
        assert_eq!(o.votes[0].vote_reason.as_deref(), Some("dup"));
        assert_eq!(o.votes[0].vote_note.as_deref(), Some("same story as the Willison post"));
        assert_eq!(o.votes[0].at, "2026-09-13T11:26:10.482-03:00");
    }

    #[test]
    fn open_outbox_parses_camel_case_and_tolerates_an_empty_list() {
        let o: OpenOutbox = serde_json::from_str(
            r#"{"opens":[{"openId":"o1","itemId":"i1","url":"https://example.com/x",
                          "at":"2026-09-13T11:26:10.482-03:00"}]}"#,
        )
        .unwrap();
        assert_eq!(o.opens[0].open_id, "o1");
        assert_eq!(o.opens[0].url, "https://example.com/x");
        let empty: OpenOutbox = serde_json::from_str("{}").unwrap();
        assert!(empty.opens.is_empty());
    }

    async fn ingest(json: &str, file: &str) -> (crate::store::testdb::Fixture, tempfile::TempDir) {
        let f = crate::store::testdb::fixture().await;
        crate::store::testdb::add_item(&f.pool, "i1").await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(file), json).unwrap();
        (f, dir)
    }

    #[tokio::test]
    async fn a_reason_entry_written_after_the_vote_wins_the_timestamp_tie() {
        // Both entries carry the same `at` — the reader picked the reason in
        // the sheet that opened on the downvote. File order decides.
        let (f, dir) = ingest(
            r#"{"votes":[
                 {"voteId":"v1","itemId":"i1","vote":-1,"at":"2026-09-13T11:26:10-03:00"},
                 {"voteId":"v2","itemId":"i1","vote":-1,"at":"2026-09-13T11:26:10-03:00",
                  "voteReason":"dup","voteNote":"same story as the other one"}
               ]}"#,
            VOTES_FILE,
        )
        .await;
        assert_eq!(ingest_votes(&f.pool, dir.path()).await.unwrap(), 2);
        assert_eq!(
            store::testdb::effective(&f.pool, "i1").await,
            (-1, Some("dup".into()), Some("same story as the other one".into()))
        );
        assert_eq!(ingest_votes(&f.pool, dir.path()).await.unwrap(), 0, "replay adds nothing");
    }

    #[tokio::test]
    async fn an_unrecognised_reason_key_keeps_the_vote_and_drops_the_label() {
        let (f, dir) = ingest(
            r#"{"votes":[{"voteId":"v1","itemId":"i1","vote":-1,
                          "at":"2026-09-13T11:26:10-03:00","voteReason":"boring"}]}"#,
            VOTES_FILE,
        )
        .await;
        assert_eq!(ingest_votes(&f.pool, dir.path()).await.unwrap(), 1);
        assert_eq!(store::testdb::effective(&f.pool, "i1").await, (-1, None, None));
    }

    #[tokio::test]
    async fn an_overlong_note_is_truncated_not_rejected() {
        let long = "x".repeat(store::MAX_VOTE_NOTE_CHARS + 50);
        let (f, dir) = ingest(
            &format!(
                r#"{{"votes":[{{"voteId":"v1","itemId":"i1","vote":-1,
                                "at":"2026-09-13T11:26:10Z","voteReason":"other",
                                "voteNote":"{long}"}}]}}"#
            ),
            VOTES_FILE,
        )
        .await;
        ingest_votes(&f.pool, dir.path()).await.unwrap();
        let (_, _, note) = store::testdb::effective(&f.pool, "i1").await;
        assert_eq!(note.unwrap().chars().count(), store::MAX_VOTE_NOTE_CHARS);
    }

    #[tokio::test]
    async fn opens_ingest_once_and_a_missing_file_is_not_a_failure() {
        let (f, dir) = ingest(
            r#"{"opens":[
                 {"openId":"o1","itemId":"i1","url":"https://example.com/i1",
                  "at":"2026-09-13T11:30:00.482-03:00"},
                 {"openId":"o1","itemId":"i1","url":"https://example.com/i1",
                  "at":"2026-09-13T11:30:00.482-03:00"}
               ]}"#,
            OPENS_FILE,
        )
        .await;
        assert_eq!(ingest_opens(&f.pool, dir.path()).await.unwrap(), 1, "duplicate openId in one file");
        assert_eq!(ingest_opens(&f.pool, dir.path()).await.unwrap(), 0, "replay adds nothing");

        let empty = tempfile::tempdir().unwrap();
        assert_eq!(ingest_opens(&f.pool, empty.path()).await.unwrap(), 0);
        assert_eq!(ingest_outboxes(&f.pool, empty.path()).await.unwrap(), 0);
    }

    #[test]
    fn every_reason_the_widget_offers_is_one_the_store_accepts() {
        for key in ["dup", "old", "knew-it", "off-topic", "weak-piece", "other"] {
            assert!(store::is_known_vote_reason(key), "{key}");
        }
        assert!(!store::is_known_vote_reason("dupe"));
        assert!(!store::is_known_vote_reason(""));
    }
}
