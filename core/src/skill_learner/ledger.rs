//! The reviewed-sessions ledger and the review-run log (ADR-038).
//!
//! One `reviewed_sessions` row per session the learner has looked at:
//! `position` is how far it has read (transcript: complete lines; session
//! index: the next turn `seq`), so a session that continued later is read
//! again only from there. A part that did not pass the pre-filter is
//! recorded the same way, so it is not evaluated again unless the session
//! grows. Sessions skipped for their agent label, and the learner's own
//! review sessions, get a row too.

use anyhow::Result;
use sqlx::SqlitePool;
use std::collections::HashMap;

/// Where a session's text was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// The Claude Code transcript JSONL (text, tool calls, tool results).
    Transcript,
    /// `memory/session_index.db` (text only), for sessions whose transcript
    /// Claude Code has deleted.
    Index,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Transcript => "transcript",
            Source::Index => "index",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "transcript" => Some(Source::Transcript),
            "index" => Some(Source::Index),
            _ => None,
        }
    }
}

/// The last thing that happened to a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The part up to `position` was reviewed.
    Reviewed,
    /// The part up to `position` did not pass the pre-filter.
    Filtered,
    /// The session's agent label is on the skip list.
    SkippedAgent,
    /// A review session of the learner itself.
    Own,
    /// The review of the part starting at `position` failed.
    Failed,
    /// The part up to `position` was given up after repeated failures.
    GaveUp,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Reviewed => "reviewed",
            Outcome::Filtered => "filtered",
            Outcome::SkippedAgent => "skipped-agent",
            Outcome::Own => "own",
            Outcome::Failed => "failed",
            Outcome::GaveUp => "gave-up",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LedgerRow {
    pub session_id: String,
    pub source: Source,
    pub agent: Option<String>,
    pub position: i64,
    /// Transcript size in bytes when the row was written (transcripts only).
    pub file_size: Option<i64>,
    pub outcome: String,
    pub reviews: i64,
    pub failures: i64,
    /// Transcripts: SHA-256 of the file's first line when it was read.
    pub head_hash: Option<String>,
    /// Transcripts: user and assistant text turns before `position`, the
    /// matching position in the session index.
    pub text_turns: Option<i64>,
}

/// How far a session has been read, as [`advance`] records it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mark<'a> {
    pub position: i64,
    /// Transcripts, when `position` is the end of the file: its size.
    pub file_size: Option<i64>,
    pub head_hash: Option<&'a str>,
    pub text_turns: Option<i64>,
}

/// Every ledger row, by session id.
pub async fn all(pool: &SqlitePool) -> Result<HashMap<String, LedgerRow>> {
    use sqlx::Row;
    let rows = sqlx::query(
        "SELECT session_id, source, agent, position, file_size, outcome, reviews, failures,
                head_hash, text_turns
           FROM reviewed_sessions",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| {
            let source = Source::parse(&r.get::<String, _>("source"))?;
            let session_id: String = r.get("session_id");
            Some((
                session_id.clone(),
                LedgerRow {
                    session_id,
                    source,
                    agent: r.get("agent"),
                    position: r.get("position"),
                    file_size: r.get("file_size"),
                    outcome: r.get("outcome"),
                    reviews: r.get("reviews"),
                    failures: r.get("failures"),
                    head_hash: r.get("head_hash"),
                    text_turns: r.get("text_turns"),
                },
            ))
        })
        .collect())
}

/// Record that `session_id` has been read up to `position` with `outcome`
/// (anything but [`Outcome::Failed`]). Resets the failure count; a review
/// also counts one more review.
pub async fn advance(
    pool: &SqlitePool,
    session_id: &str,
    source: Source,
    agent: Option<&str>,
    mark: &Mark<'_>,
    outcome: Outcome,
    signals: Option<&str>,
) -> Result<()> {
    debug_assert!(outcome != Outcome::Failed);
    let reviewed = i64::from(outcome == Outcome::Reviewed);
    sqlx::query(
        "INSERT INTO reviewed_sessions
            (session_id, source, agent, position, file_size, outcome, reviews, failures, signals,
             updated_at, head_hash, text_turns)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, ?9, ?10, ?11)
         ON CONFLICT(session_id) DO UPDATE SET
            source = excluded.source, agent = excluded.agent, position = excluded.position,
            file_size = excluded.file_size, outcome = excluded.outcome,
            reviews = reviews + excluded.reviews, failures = 0,
            signals = COALESCE(excluded.signals, signals), updated_at = excluded.updated_at,
            head_hash = excluded.head_hash, text_turns = excluded.text_turns",
    )
    .bind(session_id)
    .bind(source.as_str())
    .bind(agent)
    .bind(mark.position)
    .bind(mark.file_size)
    .bind(outcome.as_str())
    .bind(reviewed)
    .bind(signals)
    .bind(crate::timestamp::now())
    .bind(mark.head_hash)
    .bind(mark.text_turns)
    .execute(pool)
    .await?;
    Ok(())
}

/// Record a failed review of the part of `session_id` that starts at
/// `position` (the position does not move). Returns the number of
/// consecutive failures now recorded.
pub async fn record_failure(
    pool: &SqlitePool,
    session_id: &str,
    source: Source,
    agent: Option<&str>,
    position: i64,
) -> Result<i64> {
    let failures: i64 = sqlx::query_scalar(
        "INSERT INTO reviewed_sessions
            (session_id, source, agent, position, file_size, outcome, reviews, failures, updated_at)
         VALUES (?1, ?2, ?3, ?4, NULL, 'failed', 0, 1, ?5)
         ON CONFLICT(session_id) DO UPDATE SET
            outcome = 'failed', failures = failures + 1, updated_at = excluded.updated_at
         RETURNING failures",
    )
    .bind(session_id)
    .bind(source.as_str())
    .bind(agent)
    .bind(position)
    .bind(crate::timestamp::now())
    .fetch_one(pool)
    .await?;
    Ok(failures)
}

/// Record `session_id` as one of the learner's own review sessions, so no
/// later sweep reviews it.
pub async fn mark_own(pool: &SqlitePool, session_id: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO reviewed_sessions (session_id, source, position, outcome, updated_at)
         VALUES (?1, 'transcript', 0, 'own', ?2)
         ON CONFLICT(session_id) DO UPDATE SET outcome = 'own', updated_at = excluded.updated_at",
    )
    .bind(session_id)
    .bind(crate::timestamp::now())
    .execute(pool)
    .await?;
    Ok(())
}

/// One review session the learner ran, as recorded in `review_runs`.
pub struct RunStart<'a> {
    pub pass: &'a str,
    pub session_id: &'a str,
    pub source: Source,
    pub from_pos: i64,
    pub to_pos: i64,
}

/// Insert a `running` review-run row; returns its id.
pub async fn start_run(pool: &SqlitePool, r: &RunStart<'_>) -> Result<i64> {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO review_runs (pass, session_id, source, from_pos, to_pos, outcome, started_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'running', ?6) RETURNING id",
    )
    .bind(r.pass)
    .bind(r.session_id)
    .bind(r.source.as_str())
    .bind(r.from_pos)
    .bind(r.to_pos)
    .bind(crate::timestamp::now())
    .fetch_one(pool)
    .await?;
    Ok(id)
}

/// Close a review-run row.
pub async fn finish_run(
    pool: &SqlitePool,
    id: i64,
    outcome: &str,
    review_session_id: Option<&str>,
    created: i64,
    evidence_added: i64,
    detail: Option<&str>,
) -> Result<()> {
    sqlx::query(
        "UPDATE review_runs SET outcome = ?2, review_session_id = ?3, created = ?4,
                evidence_added = ?5, detail = ?6, finished_at = ?7
          WHERE id = ?1",
    )
    .bind(id)
    .bind(outcome)
    .bind(review_session_id)
    .bind(created)
    .bind(evidence_added)
    .bind(detail)
    .bind(crate::timestamp::now())
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn advance_failure_and_own_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = super::super::open(tmp.path()).await.unwrap();

        let m = |position, file_size| Mark { position, file_size, head_hash: Some("h"), text_turns: Some(position / 2) };
        advance(&pool, "s1", Source::Transcript, None, &m(40, Some(9000)), Outcome::Filtered, Some("{}"))
            .await
            .unwrap();
        advance(&pool, "s1", Source::Transcript, None, &m(90, Some(20000)), Outcome::Reviewed, None)
            .await
            .unwrap();
        assert_eq!(record_failure(&pool, "s1", Source::Transcript, None, 90).await.unwrap(), 1);
        assert_eq!(record_failure(&pool, "s1", Source::Transcript, None, 90).await.unwrap(), 2);
        mark_own(&pool, "own-1").await.unwrap();
        assert_eq!(record_failure(&pool, "s2", Source::Index, Some("whatsapp"), 3).await.unwrap(), 1);

        let rows = all(&pool).await.unwrap();
        let s1 = &rows["s1"];
        assert_eq!((s1.position, s1.file_size, s1.reviews, s1.failures), (90, Some(20000), 1, 2));
        assert_eq!(s1.outcome, "failed");
        assert_eq!(rows["own-1"].outcome, "own");
        let s2 = &rows["s2"];
        assert_eq!((s2.source, s2.position, s2.agent.as_deref()), (Source::Index, 3, Some("whatsapp")));

        // A successful advance clears the failure count.
        advance(&pool, "s1", Source::Transcript, None, &m(120, Some(25000)), Outcome::Reviewed, None)
            .await
            .unwrap();
        let s1 = &all(&pool).await.unwrap()["s1"];
        assert_eq!((s1.position, s1.reviews, s1.failures), (120, 2, 0));
        assert_eq!((s1.head_hash.as_deref(), s1.text_turns), (Some("h"), Some(60)));

        let id = start_run(
            &pool,
            &RunStart { pass: "live", session_id: "s1", source: Source::Transcript, from_pos: 90, to_pos: 120 },
        )
        .await
        .unwrap();
        finish_run(&pool, id, "ok", Some("rev-1"), 1, 0, None).await.unwrap();
        let (outcome, created): (String, i64) =
            sqlx::query_as("SELECT outcome, created FROM review_runs WHERE id = ?1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!((outcome.as_str(), created), ("ok", 1));
    }
}
