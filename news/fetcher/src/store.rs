//! `memory/news.db` — schema, migrations, and every query the fetcher runs.
//! This crate owns the file (ADR-020); the dashboard reads it.

use anyhow::Result;
use chrono::Utc;
use nucleus_core::migrate::{Migration, Step};
use nucleus_core::timestamp;
use sqlx::SqlitePool;
use std::collections::HashSet;

use crate::feed::{ParsedItem, SourceRow};

/// Versioned migrations (ADR-020).
///
/// v1 is the historical baseline, kept verbatim so existing DBs get their
/// ledger row. v2 is the ADR-031 rebuild: it wipes the item/vote/run history
/// on purpose — the old rows carry URL-only identity, discord posting state
/// and a vote schema that can't express "the operator changed their mind",
/// and the operator asked for a clean start rather than a backfill.
pub const MIGRATIONS: &[Migration] = &[
    Migration { version: 1, name: "baseline-news", step: Step::Rust(baseline_v1) },
    Migration { version: 2, name: "profile-ranking-and-widget-delivery", step: Step::Sql(V2_SQL) },
    Migration {
        version: 3,
        name: "brief-length-diagnostic",
        step: Step::Sql(
            "ALTER TABLE fetcher_runs ADD COLUMN brief_too_long INTEGER NOT NULL DEFAULT 0",
        ),
    },
    Migration { version: 4, name: "vote-reasons-opens-and-brief-inputs", step: Step::Sql(V4_SQL) },
    Migration { version: 5, name: "sortable-vote-timestamps", step: Step::Rust(normalize_vote_timestamps) },
];

fn baseline_v1(pool: &SqlitePool) -> futures::future::BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS sources (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 name TEXT UNIQUE NOT NULL,
                 url TEXT NOT NULL,
                 kind TEXT NOT NULL DEFAULT 'feed',
                 enabled INTEGER NOT NULL DEFAULT 1,
                 last_fetched_at TEXT,
                 last_error TEXT
               )"#,
        )
        .execute(pool)
        .await?;
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS items (
                 id TEXT PRIMARY KEY,
                 source_id INTEGER NOT NULL REFERENCES sources(id),
                 url TEXT UNIQUE NOT NULL,
                 title TEXT NOT NULL,
                 summary TEXT,
                 published_at TEXT NOT NULL,
                 published_date TEXT NOT NULL,
                 fetched_at TEXT NOT NULL,
                 notable_score REAL,
                 notable_reason TEXT,
                 posted_to_discord INTEGER NOT NULL DEFAULT 0
               )"#,
        )
        .execute(pool)
        .await?;
        let _ = sqlx::query("ALTER TABLE items ADD COLUMN fetch_date TEXT NOT NULL DEFAULT ''")
            .execute(pool)
            .await;
        let _ = sqlx::query("ALTER TABLE items ADD COLUMN article_url TEXT").execute(pool).await;
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS votes (
                 item_id TEXT NOT NULL REFERENCES items(id),
                 vote INTEGER NOT NULL,
                 created_at TEXT NOT NULL,
                 PRIMARY KEY(item_id, created_at)
               )"#,
        )
        .execute(pool)
        .await?;
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS fetcher_runs (
                 run_id TEXT PRIMARY KEY,
                 started_at TEXT NOT NULL,
                 finished_at TEXT,
                 items_new INTEGER NOT NULL DEFAULT 0,
                 items_notable INTEGER NOT NULL DEFAULT 0,
                 ok INTEGER NOT NULL DEFAULT 0,
                 error TEXT
               )"#,
        )
        .execute(pool)
        .await?;
        Ok(())
    })
}

/// Rebuild, not migrate. `sources` is the only table that survives.
///
/// Drop order matters: the runner wraps this in a transaction, where `PRAGMA
/// foreign_keys` is a no-op, so enforcement stays on. `votes` references
/// `items`, so it goes first and `items` is childless by the time it drops.
const V2_SQL: &str = r#"
DROP TABLE IF EXISTS votes;
DROP TABLE IF EXISTS items;
DROP TABLE IF EXISTS fetcher_runs;

CREATE TABLE items (
  id             TEXT PRIMARY KEY,
  source_id      INTEGER NOT NULL REFERENCES sources(id),
  url            TEXT NOT NULL,
  article_url    TEXT,
  canonical_url  TEXT NOT NULL,
  title          TEXT NOT NULL,
  summary        TEXT,
  published_at   TEXT NOT NULL,
  published_date TEXT NOT NULL,
  fetched_at     TEXT NOT NULL,
  fetch_date     TEXT NOT NULL,
  notable_score  REAL,
  notable_reason TEXT,
  event_slug     TEXT,
  stale          INTEGER NOT NULL DEFAULT 0,
  profile_hash   TEXT,
  prompt_version TEXT
);

CREATE UNIQUE INDEX idx_items_canonical_url ON items(canonical_url);
CREATE INDEX idx_items_fetched_at ON items(fetched_at);
CREATE INDEX idx_items_fetch_date ON items(fetch_date);
CREATE INDEX idx_items_notable_score ON items(notable_score);

CREATE TABLE votes (
  vote_id    TEXT PRIMARY KEY,
  item_id    TEXT NOT NULL REFERENCES items(id),
  vote       INTEGER NOT NULL CHECK(vote IN (-1,0,1)),
  origin     TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE INDEX idx_votes_item ON votes(item_id, created_at);

CREATE TABLE fetcher_runs (
  run_id             TEXT PRIMARY KEY,
  started_at         TEXT NOT NULL,
  finished_at        TEXT,
  ok                 INTEGER NOT NULL DEFAULT 0,
  error              TEXT,
  items_input        INTEGER NOT NULL DEFAULT 0,
  rejected_stale     INTEGER NOT NULL DEFAULT 0,
  rejected_dup_url   INTEGER NOT NULL DEFAULT 0,
  rejected_dup_title INTEGER NOT NULL DEFAULT 0,
  items_ranked       INTEGER NOT NULL DEFAULT 0,
  items_surfaced     INTEGER NOT NULL DEFAULT 0,
  brief_ok           INTEGER NOT NULL DEFAULT 0,
  profile_hash       TEXT
);

CREATE TABLE briefs (
  run_id     TEXT PRIMARY KEY REFERENCES fetcher_runs(run_id),
  created_at TEXT NOT NULL,
  text       TEXT NOT NULL
)
"#;

/// Vote reasons, click-throughs, and the provenance a brief needs to be
/// reusable.
///
/// `opens` carries no foreign key, unlike `votes`. An open is a fact about
/// what the reader did with his attention and it is worth keeping even if the
/// item it names is missing from this database — the monthly review reads it
/// as a count, and `opened` is an EXISTS lookup that an orphan can only fail.
///
/// `briefs.item_ids` is the JSON array of item ids a brief was written from.
/// Without it there is no way to answer "does the brief I'm about to reuse
/// name something he has since rejected", and reusing one that does is the
/// failure this column exists to prevent.
const V4_SQL: &str = r#"
ALTER TABLE votes ADD COLUMN reason_key TEXT;
ALTER TABLE votes ADD COLUMN note TEXT;
ALTER TABLE briefs ADD COLUMN item_ids TEXT;
ALTER TABLE fetcher_runs ADD COLUMN brief_dropped_downvoted INTEGER NOT NULL DEFAULT 0;

CREATE TABLE opens (
  open_id    TEXT PRIMARY KEY,
  item_id    TEXT NOT NULL,
  url        TEXT NOT NULL,
  origin     TEXT NOT NULL,
  created_at TEXT NOT NULL
);

CREATE INDEX idx_opens_item ON opens(item_id)
"#;

/// Rewrite stored vote timestamps into the canonical sortable form
/// (`nucleus_core::timestamp`).
///
/// The effective vote is "the latest row", and until now "latest" was a byte
/// comparison over strings two different producers wrote in two different
/// shapes — the widget in local time with an offset, the dashboard in UTC with
/// nanoseconds. With reasons arriving as *new* vote rows seconds after the
/// vote they annotate, that ordering became load-bearing.
///
/// Idempotent: normalizing an already-normalized stamp is the identity.
fn normalize_vote_timestamps(pool: &SqlitePool) -> futures::future::BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT vote_id, created_at FROM votes").fetch_all(pool).await?;
        let mut tx = pool.begin().await?;
        for (vote_id, created_at) in rows {
            let normalized = timestamp::to_sortable(&created_at);
            if normalized != created_at {
                sqlx::query("UPDATE votes SET created_at = ?1 WHERE vote_id = ?2")
                    .bind(&normalized)
                    .bind(&vote_id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(())
    })
}

/// The reasons a downvote may carry. Closed set: the widget offers exactly
/// these, and anything else arriving in the outbox is a version skew, stored
/// as NULL rather than as a value no consumer knows how to read.
///
/// `dup` and `old` are claims about the *pipeline* — they feed the dedup and
/// staleness work directly. The rest are claims about taste and only ever
/// reach the monthly profile review, as patterns rather than as rules.
pub const VOTE_REASONS: &[&str] = &["dup", "old", "knew-it", "off-topic", "weak-piece", "other"];

/// Free-text notes are a scratchpad, not a document. Longer than this and
/// something is pasting into the field.
pub const MAX_VOTE_NOTE_CHARS: usize = 500;

pub fn is_known_vote_reason(key: &str) -> bool {
    VOTE_REASONS.contains(&key)
}

/// Default feed set, re-applied on every startup so a new default picks up
/// without touching the DB by hand. `INSERT OR IGNORE` on the UNIQUE name
/// leaves operator-added sources alone.
const DEFAULT_SOURCES: &[(&str, &str)] = &[
    ("Hacker News", "https://hnrss.org/frontpage"),
    ("lobste.rs", "https://lobste.rs/rss"),
    ("Simon Willison", "https://simonwillison.net/atom/everything/"),
    ("The Pragmatic Engineer", "https://newsletter.pragmaticengineer.com/feed"),
    ("Latent Space", "https://www.latent.space/feed"),
    ("Julia Evans", "https://jvns.ca/atom.xml"),
];

/// Sources retired by name. Disabled rather than deleted so their rows and
/// the history that references them survive.
///
/// ADR-031 retired the paper firehoses (arXiv, Hugging Face) and the Rust
/// blog: the first two drowned every run in volume the operator never read,
/// and Rust is a hobby stack, not a professional one. The Reddit and
/// Anthropic rows predate that and stay off for their own reasons (Reddit's
/// RSS 403s any non-OAuth client).
const RETIRED_SOURCES: &[&str] = &[
    "Rust Blog",
    "arXiv cs.AI",
    "Hugging Face Papers",
    "Hugging Face papers",
    "r/LocalLLaMA",
];

pub async fn seed_sources(pool: &SqlitePool) -> Result<()> {
    for (name, url) in DEFAULT_SOURCES {
        let res = sqlx::query(
            "INSERT OR IGNORE INTO sources (name, url, kind, enabled) VALUES (?1, ?2, 'feed', 1)",
        )
        .bind(name)
        .bind(url)
        .execute(pool)
        .await?;
        if res.rows_affected() > 0 {
            tracing::info!(source = name, "added default source");
        }
    }
    for name in RETIRED_SOURCES {
        let res = sqlx::query("UPDATE sources SET enabled = 0 WHERE name = ?1 AND enabled = 1")
            .bind(name)
            .execute(pool)
            .await?;
        if res.rows_affected() > 0 {
            tracing::info!(source = name, "disabled retired source");
        }
    }
    Ok(())
}

pub async fn enabled_sources(pool: &SqlitePool) -> Result<Vec<SourceRow>> {
    let rows: Vec<(i64, String, String)> =
        sqlx::query_as("SELECT id, name, url FROM sources WHERE enabled = 1 ORDER BY name")
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|(id, name, url)| SourceRow { id, name, url }).collect())
}

pub async fn mark_source_ok(pool: &SqlitePool, id: i64) -> Result<()> {
    sqlx::query("UPDATE sources SET last_fetched_at = ?1, last_error = NULL WHERE id = ?2")
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn mark_source_error(pool: &SqlitePool, id: i64, err: &str) -> Result<()> {
    sqlx::query("UPDATE sources SET last_fetched_at = ?1, last_error = ?2 WHERE id = ?3")
        .bind(Utc::now().to_rfc3339())
        .bind(err)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Canonical URLs already stored, for cross-run URL dedup.
pub async fn known_canonical_urls(pool: &SqlitePool) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT canonical_url FROM items")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(u,)| u).collect())
}

/// Titles seen in the last `days` days, for cross-run event suppression.
pub async fn recent_titles(pool: &SqlitePool, days: i64) -> Result<Vec<String>> {
    let cutoff = (Utc::now() - chrono::Duration::days(days)).to_rfc3339();
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT title FROM items WHERE fetched_at >= ?1")
            .bind(cutoff)
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|(t,)| t).collect())
}

pub async fn insert_items(pool: &SqlitePool, items: &[ParsedItem]) -> Result<usize> {
    let now = Utc::now();
    let now_iso = now.to_rfc3339();
    let fetch_date = now.format("%Y-%m-%d").to_string();
    let mut inserted = 0usize;
    let mut tx = pool.begin().await?;
    for it in items {
        let res = sqlx::query(
            r#"INSERT OR IGNORE INTO items
               (id, source_id, url, article_url, canonical_url, title, summary,
                published_at, published_date, fetched_at, fetch_date)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"#,
        )
        .bind(&it.id)
        .bind(it.source_id)
        .bind(&it.url)
        .bind(&it.article_url)
        .bind(&it.canonical_url)
        .bind(&it.title)
        .bind(&it.summary)
        .bind(&it.published_at)
        .bind(&it.published_date)
        .bind(&now_iso)
        .bind(&fetch_date)
        .execute(&mut *tx)
        .await?;
        inserted += res.rows_affected() as usize;
    }
    tx.commit().await?;
    Ok(inserted)
}

/// Ranking output for one item. Written for every ranked item — nothing is
/// pruned, the score floor only decides what reaches the widget.
pub struct Ranking<'a> {
    pub item_id: &'a str,
    pub score: f64,
    pub reason: &'a str,
    pub event_slug: &'a str,
    pub stale: bool,
}

pub async fn save_rankings(
    pool: &SqlitePool,
    rankings: &[Ranking<'_>],
    profile_hash: &str,
    prompt_version: &str,
) -> Result<()> {
    let mut tx = pool.begin().await?;
    for r in rankings {
        sqlx::query(
            r#"UPDATE items
                  SET notable_score = ?1, notable_reason = ?2, event_slug = ?3,
                      stale = ?4, profile_hash = ?5, prompt_version = ?6
                WHERE id = ?7"#,
        )
        .bind(r.score)
        .bind(r.reason)
        .bind(r.event_slug)
        .bind(i64::from(r.stale))
        .bind(profile_hash)
        .bind(prompt_version)
        .bind(r.item_id)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The effective vote per item: one row each, the latest one held.
///
/// "Latest" is `created_at` first and insertion order second. The tie-break
/// matters because a reason pick arrives as a *second* vote row for the same
/// item, sometimes stamped in the same second as the vote it annotates — and
/// rows are inserted in outbox-file order, so the later entry in the file
/// wins, which is the order the reader clicked in.
const LATEST_VOTE_SQL: &str = r#"
    SELECT item_id, vote, reason_key, note
      FROM (SELECT v.item_id, v.vote, v.reason_key, v.note,
                   ROW_NUMBER() OVER (PARTITION BY v.item_id
                                      ORDER BY v.created_at DESC, v.rowid DESC) AS rn
              FROM votes v)
     WHERE rn = 1
"#;

/// One row of what the widget shows.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct SurfacedRow {
    pub id: String,
    pub title: String,
    pub source_name: String,
    pub canonical_url: String,
    pub published_at: String,
    pub notable_score: f64,
    pub notable_reason: String,
    pub event_slug: String,
    pub vote: i64,
    pub vote_reason: Option<String>,
    pub vote_note: Option<String>,
    pub opened: bool,
}

/// Everything fetched in the last 24 hours that ranked above the floor and
/// isn't flagged stale, newest-and-best first. `vote`, `vote_reason` and
/// `vote_note` all come from the one effective vote; `opened` is whether the
/// reader has ever clicked through to the item.
pub async fn surfaced_items(
    pool: &SqlitePool,
    min_score: f64,
    window: chrono::Duration,
) -> Result<Vec<SurfacedRow>> {
    let cutoff = (Utc::now() - window).to_rfc3339();
    let rows: Vec<SurfacedRow> = sqlx::query_as::<_, SurfacedRow>(&format!(
        r#"SELECT i.id,
                  i.title,
                  s.name                        AS source_name,
                  i.canonical_url,
                  i.published_at,
                  i.notable_score               AS notable_score,
                  COALESCE(i.notable_reason,'') AS notable_reason,
                  COALESCE(i.event_slug,'')     AS event_slug,
                  COALESCE(ev.vote, 0)          AS vote,
                  ev.reason_key                 AS vote_reason,
                  ev.note                       AS vote_note,
                  EXISTS(SELECT 1 FROM opens o WHERE o.item_id = i.id) AS opened
             FROM items i
             JOIN sources s ON s.id = i.source_id
             LEFT JOIN ({LATEST_VOTE_SQL}) ev ON ev.item_id = i.id
            WHERE i.fetched_at >= ?1
              AND i.stale = 0
              AND i.notable_score IS NOT NULL
              AND i.notable_score >= ?2
            ORDER BY i.notable_score DESC, i.published_at DESC"#
    ))
    .bind(cutoff)
    .bind(min_score)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Items whose effective vote is a downvote. The brief must not name any of
/// them, including through a stored brief it would otherwise reuse.
pub async fn downvoted_item_ids(pool: &SqlitePool) -> Result<HashSet<String>> {
    let rows: Vec<(String,)> =
        sqlx::query_as(&format!("SELECT item_id FROM ({LATEST_VOTE_SQL}) WHERE vote = -1"))
            .fetch_all(pool)
            .await?;
    Ok(rows.into_iter().map(|(id,)| id).collect())
}

/// One vote as it arrives from an outbox or the dashboard.
pub struct IncomingVote<'a> {
    pub vote_id: &'a str,
    pub item_id: &'a str,
    pub vote: i64,
    pub origin: &'a str,
    /// As the producer wrote it; normalized to the sortable form on the way in.
    pub created_at: &'a str,
    /// One of [`VOTE_REASONS`], or None. An unrecognised key is the caller's
    /// to reject — by the time it reaches here it is already NULL.
    pub reason_key: Option<&'a str>,
    pub note: Option<&'a str>,
}

/// Insert a vote, ignoring one we already hold under the same `vote_id`.
/// Returns false when the id was already present or the item is unknown —
/// the widget's outbox is append-only and replays freely.
pub async fn insert_vote(pool: &SqlitePool, v: &IncomingVote<'_>) -> Result<bool> {
    // Foreign keys are enforced on this pool, and IGNORE escalates an FK
    // violation to an abort — so check the referent first. A vote for an item
    // we never stored is data about nothing.
    let known: Option<(String,)> = sqlx::query_as("SELECT id FROM items WHERE id = ?1")
        .bind(v.item_id)
        .fetch_optional(pool)
        .await?;
    if known.is_none() {
        return Ok(false);
    }
    let res = sqlx::query(
        "INSERT OR IGNORE INTO votes (vote_id, item_id, vote, origin, created_at, reason_key, note)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .bind(v.vote_id)
    .bind(v.item_id)
    .bind(v.vote)
    .bind(v.origin)
    .bind(timestamp::to_sortable(v.created_at))
    .bind(v.reason_key)
    .bind(v.note)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Record that the reader opened an item. Idempotent by `open_id`.
///
/// Opens are attention, not preference: nothing downstream of this row scores,
/// ranks, suppresses or surfaces anything. It round-trips to the widget as
/// `opened` and it is evidence in the monthly review.
pub async fn insert_open(
    pool: &SqlitePool,
    open_id: &str,
    item_id: &str,
    url: &str,
    origin: &str,
    created_at: &str,
) -> Result<bool> {
    let res = sqlx::query(
        "INSERT OR IGNORE INTO opens (open_id, item_id, url, origin, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(open_id)
    .bind(item_id)
    .bind(url)
    .bind(origin)
    .bind(timestamp::to_sortable(created_at))
    .execute(pool)
    .await?;
    Ok(res.rows_affected() > 0)
}

/// Per-run diagnostics. Every counter the operator needs to answer "why did
/// today look like that" without re-reading the log.
#[derive(Debug, Default, Clone)]
pub struct RunDiagnostics {
    pub items_input: usize,
    pub rejected_stale: usize,
    pub rejected_dup_url: usize,
    pub rejected_dup_title: usize,
    pub items_ranked: usize,
    pub items_surfaced: usize,
    pub brief_ok: bool,
    /// The brief exceeded the widget's word cap twice and was discarded in
    /// favour of the previous one. Distinct from `brief_ok = false` for a
    /// session failure — this one is a prompt problem, not an infra problem.
    pub brief_too_long: bool,
    /// The brief call failed and the stored one couldn't stand in because it
    /// was written over an item the reader has since downvoted. The day went
    /// out with no brief rather than with a retracted recommendation.
    pub brief_dropped_downvoted: bool,
    pub profile_hash: String,
}

pub async fn record_run_start(pool: &SqlitePool, run_id: &str) -> Result<()> {
    sqlx::query("INSERT INTO fetcher_runs (run_id, started_at, ok) VALUES (?1, ?2, 0)")
        .bind(run_id)
        .bind(Utc::now().to_rfc3339())
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn record_run_finish(
    pool: &SqlitePool,
    run_id: &str,
    diag: &RunDiagnostics,
    error: Option<&str>,
) -> Result<()> {
    sqlx::query(
        r#"UPDATE fetcher_runs
              SET finished_at = ?1, ok = ?2, error = ?3,
                  items_input = ?4, rejected_stale = ?5, rejected_dup_url = ?6,
                  rejected_dup_title = ?7, items_ranked = ?8, items_surfaced = ?9,
                  brief_ok = ?10, brief_too_long = ?11, brief_dropped_downvoted = ?12,
                  profile_hash = ?13
            WHERE run_id = ?14"#,
    )
    .bind(Utc::now().to_rfc3339())
    .bind(i64::from(error.is_none()))
    .bind(error)
    .bind(diag.items_input as i64)
    .bind(diag.rejected_stale as i64)
    .bind(diag.rejected_dup_url as i64)
    .bind(diag.rejected_dup_title as i64)
    .bind(diag.items_ranked as i64)
    .bind(diag.items_surfaced as i64)
    .bind(i64::from(diag.brief_ok))
    .bind(i64::from(diag.brief_too_long))
    .bind(i64::from(diag.brief_dropped_downvoted))
    .bind(&diag.profile_hash)
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Store a brief together with the items it was written from. The ids are
/// what makes it reusable later — see [`last_reusable_brief`].
pub async fn save_brief(
    pool: &SqlitePool,
    run_id: &str,
    text: &str,
    item_ids: &[String],
) -> Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO briefs (run_id, created_at, text, item_ids)
         VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(run_id)
    .bind(timestamp::now())
    .bind(text)
    .bind(serde_json::to_string(item_ids)?)
    .execute(pool)
    .await?;
    Ok(())
}

/// What the last stored brief is worth to this run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BriefFallback {
    /// Safe to show again: every item it was written from is still one the
    /// reader accepts.
    Reuse(String),
    /// A brief exists, but it names something he has since downvoted — or it
    /// predates `briefs.item_ids` and there is no way to tell. Either way it
    /// is not going back on screen; the day gets an empty brief instead.
    Blocked,
    /// Nothing stored at all.
    Empty,
}

/// The fallback for a run whose brief call failed.
///
/// A stale sentence beats an empty one, but only while it is still true. A
/// brief that recommends an item the reader has since rejected is worse than
/// no brief: it is the system repeating something it was just told to stop
/// saying, and on the widget there is nothing to explain the lag.
pub async fn last_reusable_brief(pool: &SqlitePool) -> Result<BriefFallback> {
    let row: Option<(String, Option<String>)> =
        sqlx::query_as("SELECT text, item_ids FROM briefs ORDER BY created_at DESC LIMIT 1")
            .fetch_optional(pool)
            .await?;
    let Some((text, item_ids)) = row else {
        return Ok(BriefFallback::Empty);
    };
    let Some(ids) = item_ids else {
        tracing::warn!("the last brief predates input tracking — not reusing it");
        return Ok(BriefFallback::Blocked);
    };
    let ids: Vec<String> = serde_json::from_str(&ids).unwrap_or_default();
    let downvoted = downvoted_item_ids(pool).await?;
    if ids.iter().any(|id| downvoted.contains(id)) {
        return Ok(BriefFallback::Blocked);
    }
    Ok(BriefFallback::Reuse(text))
}

/// A migrated, seeded news DB in a temp dir, for tests that need real SQL
/// rather than a stub. Lives here because `widget`'s ingest tests need the
/// same fixture.
#[cfg(test)]
pub(crate) mod testdb {
    use super::*;

    pub struct Fixture {
        pub pool: SqlitePool,
        /// Held so the directory outlives the pool.
        pub _dir: tempfile::TempDir,
    }

    /// A file-backed DB, not `sqlite::memory:` — a pooled in-memory SQLite
    /// gives every connection its own empty database.
    pub async fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let pool = nucleus_core::db::open(&dir.path().join("news.db")).await.unwrap();
        nucleus_core::migrate::migrate(&pool, MIGRATIONS).await.unwrap();
        sqlx::query("INSERT INTO sources (name, url, kind, enabled) VALUES ('Test', 'x', 'feed', 1)")
            .execute(&pool)
            .await
            .unwrap();
        Fixture { pool, _dir: dir }
    }

    /// One stored item, ready to be voted on or opened.
    pub async fn add_item(pool: &SqlitePool, id: &str) {
        sqlx::query(
            "INSERT INTO items (id, source_id, url, canonical_url, title, published_at,
                                published_date, fetched_at, fetch_date, notable_score,
                                notable_reason, event_slug, stale)
             VALUES (?1, 1, ?2, ?2, ?3, '2026-09-13T09:00:00.000Z', '2026-09-13',
                     ?4, '2026-09-13', 0.8, 'r', 'ev', 0)",
        )
        .bind(id)
        .bind(format!("https://example.com/{id}"))
        .bind(format!("title {id}"))
        .bind(timestamp::now())
        .execute(pool)
        .await
        .unwrap();
    }

    pub async fn effective(pool: &SqlitePool, item_id: &str) -> (i64, Option<String>, Option<String>) {
        let rows = surfaced_items(pool, 0.0, chrono::Duration::hours(24)).await.unwrap();
        let row = rows.into_iter().find(|r| r.id == item_id).expect("item not surfaced");
        (row.vote, row.vote_reason, row.vote_note)
    }
}

#[cfg(test)]
mod tests {
    use super::testdb::*;
    use super::*;

    fn vote<'a>(
        vote_id: &'a str,
        item_id: &'a str,
        vote: i64,
        at: &'a str,
        reason: Option<&'a str>,
    ) -> IncomingVote<'a> {
        IncomingVote { vote_id, item_id, vote, origin: "widget", created_at: at, reason_key: reason, note: None }
    }

    #[tokio::test]
    async fn a_vote_is_stored_once_however_often_the_outbox_replays() {
        let f = fixture().await;
        add_item(&f.pool, "i1").await;
        let v = vote("v1", "i1", -1, "2026-09-13T11:26:10-03:00", Some("dup"));
        assert!(insert_vote(&f.pool, &v).await.unwrap(), "first insert is new");
        assert!(!insert_vote(&f.pool, &v).await.unwrap(), "replay is a no-op");
        assert_eq!(effective(&f.pool, "i1").await, (-1, Some("dup".into()), None));
    }

    #[tokio::test]
    async fn timestamps_are_stored_in_sortable_form_whatever_the_producer_wrote() {
        let f = fixture().await;
        add_item(&f.pool, "i1").await;
        insert_vote(&f.pool, &vote("v1", "i1", 1, "2026-09-13T11:26:10-03:00", None))
            .await
            .unwrap();
        let (stored,): (String,) = sqlx::query_as("SELECT created_at FROM votes WHERE vote_id = 'v1'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
        assert_eq!(stored, "2026-09-13T14:26:10.000Z");
    }

    #[tokio::test]
    async fn a_reason_cast_in_the_same_second_supersedes_the_vote_it_explains() {
        let f = fixture().await;
        add_item(&f.pool, "i1").await;
        // The widget writes a downvote, then the reason pick as a second
        // entry — same item, same vote, same second, later in the file.
        let at = "2026-09-13T11:26:10-03:00";
        insert_vote(&f.pool, &vote("v1", "i1", -1, at, None)).await.unwrap();
        insert_vote(&f.pool, &vote("v2", "i1", -1, at, Some("dup"))).await.unwrap();
        assert_eq!(
            effective(&f.pool, "i1").await,
            (-1, Some("dup".into()), None),
            "the later entry in the file wins a timestamp tie"
        );
    }

    #[tokio::test]
    async fn a_later_vote_supersedes_an_earlier_one() {
        let f = fixture().await;
        add_item(&f.pool, "i1").await;
        insert_vote(&f.pool, &vote("v1", "i1", -1, "2026-09-13T11:26:10Z", Some("dup")))
            .await
            .unwrap();
        // Taking it back, written later but arriving in the same replay.
        insert_vote(&f.pool, &vote("v2", "i1", 0, "2026-09-13T11:40:00Z", None)).await.unwrap();
        assert_eq!(effective(&f.pool, "i1").await, (0, None, None));
        assert!(downvoted_item_ids(&f.pool).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_vote_for_an_item_we_never_stored_is_dropped() {
        let f = fixture().await;
        assert!(!insert_vote(&f.pool, &vote("v1", "ghost", -1, "2026-09-13T11:26:10Z", None))
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn opens_are_idempotent_and_never_touch_the_vote() {
        let f = fixture().await;
        add_item(&f.pool, "i1").await;
        let at = "2026-09-13T11:30:00.482-03:00";
        assert!(insert_open(&f.pool, "o1", "i1", "https://example.com/i1", "widget", at)
            .await
            .unwrap());
        assert!(
            !insert_open(&f.pool, "o1", "i1", "https://example.com/i1", "widget", at).await.unwrap(),
            "replaying the outbox inserts nothing"
        );
        let rows = surfaced_items(&f.pool, 0.0, chrono::Duration::hours(24)).await.unwrap();
        assert!(rows[0].opened, "the open round-trips");
        assert_eq!(rows[0].vote, 0, "an open is not a vote");
    }

    #[tokio::test]
    async fn an_open_for_an_unknown_item_is_still_recorded() {
        // No foreign key: attention data outlives the item row it names.
        let f = fixture().await;
        assert!(insert_open(&f.pool, "o1", "ghost", "https://example.com/g", "widget", "2026-09-13T11:30:00Z")
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn a_stored_brief_is_reusable_until_one_of_its_items_is_downvoted() {
        let f = fixture().await;
        add_item(&f.pool, "i1").await;
        add_item(&f.pool, "i2").await;
        sqlx::query("INSERT INTO fetcher_runs (run_id, started_at) VALUES ('r1', '2026-09-13T09:00:00.000Z')")
            .execute(&f.pool)
            .await
            .unwrap();
        save_brief(&f.pool, "r1", "Two things happened.", &["i1".into(), "i2".into()])
            .await
            .unwrap();
        assert_eq!(
            last_reusable_brief(&f.pool).await.unwrap(),
            BriefFallback::Reuse("Two things happened.".into())
        );

        insert_vote(&f.pool, &vote("v1", "i2", -1, "2026-09-13T11:26:10Z", Some("dup")))
            .await
            .unwrap();
        assert_eq!(
            last_reusable_brief(&f.pool).await.unwrap(),
            BriefFallback::Blocked,
            "a brief that names a rejected item does not go back on screen"
        );
    }

    #[tokio::test]
    async fn a_brief_from_before_input_tracking_is_not_reused() {
        let f = fixture().await;
        sqlx::query("INSERT INTO fetcher_runs (run_id, started_at) VALUES ('r1', '2026-09-13T09:00:00.000Z')")
            .execute(&f.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO briefs (run_id, created_at, text) VALUES ('r1', ?1, 'old')")
            .bind(timestamp::now())
            .execute(&f.pool)
            .await
            .unwrap();
        assert_eq!(last_reusable_brief(&f.pool).await.unwrap(), BriefFallback::Blocked);
    }

    #[tokio::test]
    async fn no_brief_at_all_is_its_own_answer() {
        let f = fixture().await;
        assert_eq!(last_reusable_brief(&f.pool).await.unwrap(), BriefFallback::Empty);
    }
}
