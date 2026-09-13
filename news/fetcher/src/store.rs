//! `memory/news.db` — schema, migrations, and every query the fetcher runs.
//! This crate owns the file (ADR-020); the dashboard reads it.

use anyhow::Result;
use chrono::Utc;
use nucleus_core::migrate::{Migration, Step};
use sqlx::SqlitePool;

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
}

/// Everything fetched in the last 24 hours that ranked above the floor and
/// isn't flagged stale, newest-and-best first. `vote` is the effective vote:
/// the latest row per item, 0 when there is none.
pub async fn surfaced_items(
    pool: &SqlitePool,
    min_score: f64,
    window: chrono::Duration,
) -> Result<Vec<SurfacedRow>> {
    let cutoff = (Utc::now() - window).to_rfc3339();
    let rows: Vec<SurfacedRow> = sqlx::query_as::<_, SurfacedRow>(
        r#"SELECT i.id,
                  i.title,
                  s.name                        AS source_name,
                  i.canonical_url,
                  i.published_at,
                  i.notable_score               AS notable_score,
                  COALESCE(i.notable_reason,'') AS notable_reason,
                  COALESCE(i.event_slug,'')     AS event_slug,
                  COALESCE((SELECT v.vote FROM votes v
                             WHERE v.item_id = i.id
                             ORDER BY v.created_at DESC, v.rowid DESC
                             LIMIT 1), 0)       AS vote
             FROM items i
             JOIN sources s ON s.id = i.source_id
            WHERE i.fetched_at >= ?1
              AND i.stale = 0
              AND i.notable_score IS NOT NULL
              AND i.notable_score >= ?2
            ORDER BY i.notable_score DESC, i.published_at DESC"#,
    )
    .bind(cutoff)
    .bind(min_score)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

/// Insert a vote, ignoring one we already hold under the same `vote_id`.
/// Returns false when the id was already present or the item is unknown —
/// the widget's outbox is append-only and replays freely.
pub async fn insert_vote(
    pool: &SqlitePool,
    vote_id: &str,
    item_id: &str,
    vote: i64,
    origin: &str,
    created_at: &str,
) -> Result<bool> {
    // Foreign keys are enforced on this pool, and IGNORE escalates an FK
    // violation to an abort — so check the referent first. A vote for an item
    // we never stored is data about nothing.
    let known: Option<(String,)> = sqlx::query_as("SELECT id FROM items WHERE id = ?1")
        .bind(item_id)
        .fetch_optional(pool)
        .await?;
    if known.is_none() {
        return Ok(false);
    }
    let res = sqlx::query(
        "INSERT OR IGNORE INTO votes (vote_id, item_id, vote, origin, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(vote_id)
    .bind(item_id)
    .bind(vote)
    .bind(origin)
    .bind(created_at)
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
                  brief_ok = ?10, profile_hash = ?11
            WHERE run_id = ?12"#,
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
    .bind(&diag.profile_hash)
    .bind(run_id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn save_brief(pool: &SqlitePool, run_id: &str, text: &str) -> Result<()> {
    sqlx::query("INSERT OR REPLACE INTO briefs (run_id, created_at, text) VALUES (?1, ?2, ?3)")
        .bind(run_id)
        .bind(Utc::now().to_rfc3339())
        .bind(text)
        .execute(pool)
        .await?;
    Ok(())
}

/// The last brief we managed to write. Used as the fallback when today's
/// brief call fails — a stale sentence beats an empty one.
pub async fn last_brief(pool: &SqlitePool) -> Result<Option<String>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT text FROM briefs ORDER BY created_at DESC LIMIT 1")
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(t,)| t))
}
