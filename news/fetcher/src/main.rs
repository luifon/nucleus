//! news-fetcher — twice-daily AI/tech news pull.
//!
//! Pipeline: load sources → fetch RSS/Atom → dedupe by URL → score notable via
//! claude → store → post top notable items to Discord.

use anyhow::{Context, Result};
use chrono::Utc;
use nucleus_core::{
    claude::PermissionMode,
    claude_session::{AskOptions, Session, SpawnOptions},
    config::Settings,
    db, diary, discord_sdk,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::path::PathBuf;

const AGENT_NAME: &str = "news-fetcher";
const DB_PATH: &str = "memory/news.db";
const TOP_N_NOTABLE_TO_POST: usize = 5;
// Two-stage per-source bound: score at most PRE_SCORE_CAP new items (slice
// taken in RSS-feed order, which is newest-first for the feeds we pull),
// then keep at most POST_SCORE_CAP. Keeps the LLM scoring call bounded
// even when a firehose (arXiv cs.AI) drops hundreds of new items in one
// pull — without this, the per-source score_all() prompt grows unbounded.
const PRE_SCORE_CAP: usize = 20;
const POST_SCORE_CAP: usize = 10;

#[derive(Debug)]
struct SourceRow {
    id: i64,
    name: String,
    url: String,
}

#[derive(Debug, Clone)]
struct ParsedItem {
    id: String,
    source_id: i64,
    /// The URL we want clicks to land on. For curation sites (HN,
    /// lobste.rs) this is the discussion page, not the underlying article.
    url: String,
    /// The underlying primary-source URL when `url` is a discussion page.
    /// Shown as a small "↗ original" chip on the card.
    article_url: Option<String>,
    title: String,
    summary: Option<String>,
    published_at: String,
    published_date: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct ScoredItem {
    id: String,
    score: f64,
    reason: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    nucleus_core::init_tracing();
    let settings = Settings::load().context("loading nucleus.toml + .env")?;
    let workspace_root = std::env::current_dir()?;
    let pool = db::open(&workspace_root.join(DB_PATH)).await?;

    let _ = tokio::process::Command::new("tmux")
        .args(["kill-session", "-t", "nucleus-news-fetcher"])
        .output()
        .await;

    nucleus_core::migrate::migrate(&pool, MIGRATIONS)
        .await
        .context("migrating news.db")?;
    // Data seeding (not schema) — stays outside migrations by design:
    // INSERT OR IGNORE re-runs every boot so new defaults pick up.
    seed_default_sources(&pool).await?;

    // Subcommand dispatch. With no arg we do the normal twice-a-day fetch.
    // `rescore-today` is a backfill — apply the new schema + cap policy
    // to items already in the DB for today, without re-fetching.
    match std::env::args().nth(1).as_deref() {
        Some("rescore-today") => {
            return rescore_today(&pool, &workspace_root, &settings.identity.user_name).await;
        }
        Some(other) => {
            anyhow::bail!("unknown subcommand: {other}");
        }
        None => {}
    }

    let run_id = uuid::Uuid::new_v4().to_string();
    record_run_start(&pool, &run_id).await?;

    let sources = list_enabled_sources(&pool).await?;
    tracing::info!("fetcher: pulling {} sources", sources.len());

    let http = reqwest::Client::builder()
        // Reddit's RSS endpoints reject the default reqwest UA AND any
        // string that looks botty (anything ending in /<version>). A
        // browser-like UA is the path of least resistance for the sources
        // that share Reddit's anti-bot posture (lobste.rs, GitHub feeds).
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) AppleWebKit/537.36 (KHTML, like Gecko) nucleus-news-fetcher/0.1")
        .timeout(std::time::Duration::from_secs(20))
        .build()?;

    let mut all_kept: Vec<ParsedItem> = Vec::new();
    let mut notable_count: usize = 0;
    for src in &sources {
        match fetch_source(&http, src).await {
            Ok(items) => {
                let new = upsert_items(&pool, &items).await?;
                tracing::info!("fetcher: {} -> {} parsed, {} new", src.name, items.len(), new.len());
                mark_source_ok(&pool, src.id).await?;
                if new.is_empty() {
                    continue;
                }
                // Two-stage cap: take the first PRE_SCORE_CAP items (RSS
                // order = newest-first) to bound the LLM scoring call, then
                // keep at most POST_SCORE_CAP by notable_score. Everything
                // else (including the unscored 21..N) gets deleted by
                // persist_top_n so the UI isn't drowned by a single source.
                let to_score: &[ParsedItem] = &new[..new.len().min(PRE_SCORE_CAP)];
                let scored = score_all(&workspace_root, to_score, &settings.identity.user_name).await?;
                let kept = persist_top_n(&pool, &new, &scored, POST_SCORE_CAP).await?;
                notable_count += scored.iter().filter(|s| s.score >= 0.6 && kept.iter().any(|k| k.id == s.id)).count();
                all_kept.extend(kept);
            }
            Err(e) => {
                tracing::warn!("fetcher: {} failed: {}", src.name, e);
                mark_source_error(&pool, src.id, &e.to_string()).await?;
            }
        }
    }

    record_run_finish(&pool, &run_id, all_kept.len(), notable_count).await?;

    // Post top notable to Discord.
    let posted = post_top_notable(
        &pool,
        &settings.discord.home_channel_id,
        TOP_N_NOTABLE_TO_POST,
        settings.public_urls.nucleus.as_deref(),
    )
    .await?;
    tracing::info!("fetcher: posted {} notable items to discord", posted);

    let _ = diary::record_observation(
        &workspace_root,
        AGENT_NAME,
        "fetcher run",
        &format!(
            "ok: {} new items kept (after per-source cap), {} notable (>=0.6), {} posted",
            all_kept.len(),
            notable_count,
            posted
        ),
        diary::Tag::Observation,
    );

    Ok(())
}

/// One-off backfill: take today's items in-place, apply the new pipeline
/// (HN/lobste.rs URL swap from summary, then score-everything-and-cap-to-N).
/// Run via `news-fetcher rescore-today`. No network fetch happens.
async fn rescore_today(pool: &SqlitePool, workspace_root: &PathBuf, user_name: &str) -> Result<()> {
    let today = Utc::now().format("%Y-%m-%d").to_string();
    tracing::info!("rescore-today: target fetch_date = {}", today);

    let sources = list_enabled_sources(pool).await?;
    for src in &sources {
        // Load every item from this source for today.
        let rows: Vec<(String, String, Option<String>, String, Option<String>, String, String, Option<f64>)> =
            sqlx::query_as(
                "SELECT id, url, article_url, title, summary, published_at, published_date, notable_score
                 FROM items WHERE source_id = ?1 AND fetch_date = ?2",
            )
            .bind(src.id)
            .bind(&today)
            .fetch_all(pool)
            .await?;
        if rows.is_empty() {
            continue;
        }
        tracing::info!("rescore-today: {} -> {} items in DB", src.name, rows.len());

        // Step A — for HN/lobste.rs items, re-parse the summary to lift
        // the discussion URL out and swap with the article URL.
        for (id, url, article_url, _title, summary, _pa, _pd, _score) in &rows {
            if !matches!(src.name.as_str(), "Hacker News" | "lobste.rs") {
                continue;
            }
            // The pick_primary_url helper wants the raw (unsanitized) body
            // so the "Comments URL:" line survives. Existing rows hold the
            // sanitized version; the URL still appears as plain text so
            // the same line-scan works.
            let (new_url, new_article) = pick_primary_url(&src.name, url, summary.as_deref());
            if new_url != *url || new_article.as_ref() != article_url.as_ref() {
                // Defensive: skip if the new_url collides with another row.
                let conflict: Option<(String,)> = sqlx::query_as(
                    "SELECT id FROM items WHERE url = ?1 AND id <> ?2",
                )
                .bind(&new_url)
                .bind(id)
                .fetch_optional(pool)
                .await?;
                if conflict.is_some() {
                    continue;
                }
                sqlx::query("UPDATE items SET url = ?1, article_url = ?2 WHERE id = ?3")
                    .bind(&new_url)
                    .bind(&new_article)
                    .bind(id)
                    .execute(pool)
                    .await?;
            }
        }

        // Step B — score the first PRE_SCORE_CAP unscored items only.
        // Same bound as the main fetch path: arXiv-shape days can leave
        // hundreds of unscored rows, and `score_all` is one Claude call
        // for the whole batch — must stay bounded.
        let to_score_rows: Vec<(String, String, Option<String>)> = sqlx::query_as(
            "SELECT id, title, summary FROM items
             WHERE source_id = ?1 AND fetch_date = ?2 AND notable_score IS NULL
             ORDER BY published_at DESC
             LIMIT ?3",
        )
        .bind(src.id)
        .bind(&today)
        .bind(PRE_SCORE_CAP as i64)
        .fetch_all(pool)
        .await?;
        let to_score: Vec<ParsedItem> = to_score_rows
            .into_iter()
            .map(|(id, title, summary)| ParsedItem {
                id,
                source_id: src.id,
                url: String::new(),
                article_url: None,
                title,
                summary,
                published_at: String::new(),
                published_date: String::new(),
            })
            .collect();
        if !to_score.is_empty() {
            tracing::info!("rescore-today: {} -> scoring {} unscored items", src.name, to_score.len());
            let scored = score_all(workspace_root, &to_score, user_name).await?;
            for s in &scored {
                save_score(pool, &s.id, s.score, &s.reason).await?;
            }
        }

        // Step C — prune to top POST_SCORE_CAP by score. NULL scores sort
        // last (treated as 0 in the ORDER BY).
        let cap = POST_SCORE_CAP as i64;
        let to_drop: Vec<(String,)> = sqlx::query_as(
            "SELECT id FROM items
              WHERE source_id = ?1 AND fetch_date = ?2
              ORDER BY COALESCE(notable_score, 0) DESC, published_at DESC
              LIMIT -1 OFFSET ?3",
        )
        .bind(src.id)
        .bind(&today)
        .bind(cap)
        .fetch_all(pool)
        .await?;
        if !to_drop.is_empty() {
            let drop_ids: Vec<String> = to_drop.into_iter().map(|(id,)| id).collect();
            tracing::info!("rescore-today: {} -> dropping {} below top {}", src.name, drop_ids.len(), cap);
            delete_items(pool, &drop_ids).await?;
        }
    }
    Ok(())
}

/// Apply scores to `new` items, sort by notable_score desc, persist the top
/// `cap` to the items table (scores written), DELETE the rest. Items that
/// Claude didn't score (e.g., batch failure) get a score of 0 so they sort
/// below scored items and get dropped first.
async fn persist_top_n(
    pool: &SqlitePool,
    new: &[ParsedItem],
    scored: &[ScoredItem],
    cap: usize,
) -> Result<Vec<ParsedItem>> {
    use std::collections::HashMap;
    let score_map: HashMap<&str, &ScoredItem> =
        scored.iter().map(|s| (s.id.as_str(), s)).collect();

    let mut ranked: Vec<(&ParsedItem, f64, Option<&str>)> = new
        .iter()
        .map(|it| {
            let (score, reason) = match score_map.get(it.id.as_str()) {
                Some(s) => (s.score, Some(s.reason.as_str())),
                None => (0.0, None),
            };
            (it, score, reason)
        })
        .collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    let (keep, drop): (Vec<_>, Vec<_>) = ranked.into_iter().enumerate().partition(|(i, _)| *i < cap);

    // Persist scores on the kept items so the UI can show them.
    for (_, (item, score, reason)) in &keep {
        save_score(pool, &item.id, *score, reason.unwrap_or("")).await?;
    }
    // Drop the rest from the items table entirely — they didn't make the cut.
    let drop_ids: Vec<String> = drop.iter().map(|(_, (it, _, _))| it.id.clone()).collect();
    if !drop_ids.is_empty() {
        delete_items(pool, &drop_ids).await?;
    }

    Ok(keep.into_iter().map(|(_, (it, _, _))| it.clone()).collect())
}

/// Versioned migrations (ADR-020): v1 = the historical ensure_schema body
/// verbatim (idempotent CREATEs + tolerated ALTERs + fetch_date backfill).
/// New schema changes go in as v2+ and run exactly once.
const MIGRATIONS: &[nucleus_core::migrate::Migration] = &[nucleus_core::migrate::Migration {
    version: 1,
    name: "baseline-news",
    step: nucleus_core::migrate::Step::Rust(baseline_v1),
}];

fn baseline_v1(pool: &SqlitePool) -> futures::future::BoxFuture<'_, Result<()>> {
    Box::pin(ensure_schema(pool))
}

async fn ensure_schema(pool: &SqlitePool) -> Result<()> {
    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS sources (
          id INTEGER PRIMARY KEY AUTOINCREMENT,
          name TEXT UNIQUE NOT NULL,
          url TEXT NOT NULL,
          kind TEXT NOT NULL DEFAULT 'feed',
          enabled INTEGER NOT NULL DEFAULT 1,
          last_fetched_at TEXT,
          last_error TEXT
        );
    "#).execute(pool).await?;
    // Note: a `max_per_fetch` column may still exist on legacy DBs from
    // before the PRE_SCORE_CAP / POST_SCORE_CAP refactor. It is no longer
    // read; left in place to avoid an irreversible schema change.

    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS items (
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
        );
    "#).execute(pool).await?;
    // Add fetch_date column (idempotent) — the date the item entered our DB,
    // independent of the article's own published_at. UI groups by this.
    let _ = sqlx::query("ALTER TABLE items ADD COLUMN fetch_date TEXT NOT NULL DEFAULT ''")
        .execute(pool).await;
    // article_url — set when items.url points at a discussion page (HN
    // comments, lobste.rs story page). Then article_url is the underlying
    // primary-source URL we surface as a small "↗ original" chip.
    let _ = sqlx::query("ALTER TABLE items ADD COLUMN article_url TEXT")
        .execute(pool).await;
    // Backfill any rows missing fetch_date — use the date portion of fetched_at.
    sqlx::query("UPDATE items SET fetch_date = substr(fetched_at, 1, 10) WHERE fetch_date = ''")
        .execute(pool).await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_items_published_date ON items(published_date)")
        .execute(pool).await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_items_fetch_date ON items(fetch_date)")
        .execute(pool).await?;
    sqlx::query("CREATE INDEX IF NOT EXISTS idx_items_notable_score ON items(notable_score)")
        .execute(pool).await?;

    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS votes (
          item_id TEXT NOT NULL REFERENCES items(id),
          vote INTEGER NOT NULL,
          created_at TEXT NOT NULL,
          PRIMARY KEY(item_id, created_at)
        );
    "#).execute(pool).await?;

    sqlx::query(r#"
        CREATE TABLE IF NOT EXISTS fetcher_runs (
          run_id TEXT PRIMARY KEY,
          started_at TEXT NOT NULL,
          finished_at TEXT,
          items_new INTEGER NOT NULL DEFAULT 0,
          items_notable INTEGER NOT NULL DEFAULT 0,
          ok INTEGER NOT NULL DEFAULT 0,
          error TEXT
        );
    "#).execute(pool).await?;

    Ok(())
}

async fn seed_default_sources(pool: &SqlitePool) -> Result<()> {
    // Idempotent — runs every startup. INSERT OR IGNORE on the UNIQUE
    // (name) constraint silently no-ops when a source already exists,
    // so we can add new defaults later and they pick up automatically
    // without disturbing the existing DB. Manual sources added via the
    // dashboard / SQL stay put.
    let defaults: &[(&str, &str)] = &[
        // (name, url) — all sources cap at PRE_SCORE_CAP / POST_SCORE_CAP
        // (currently 20 / 10) regardless of source. arXiv cs.AI publishes
        // ~400/day but the pre-score slice keeps the LLM call bounded.
        ("Hacker News",            "https://hnrss.org/frontpage"),
        ("lobste.rs",              "https://lobste.rs/rss"),
        ("Simon Willison",         "https://simonwillison.net/atom/everything/"),
        ("The Pragmatic Engineer", "https://newsletter.pragmaticengineer.com/feed"),
        ("Latent Space",           "https://www.latent.space/feed"),
        ("Anthropic Engineering",  "https://www.anthropic.com/engineering/rss.xml"),
        ("Hugging Face papers",    "https://jamesg.blog/hf-papers.xml"),
        ("Rust Blog",              "https://blog.rust-lang.org/feed.xml"),
        ("Julia Evans",            "https://jvns.ca/atom.xml"),
        ("arXiv cs.AI",            "https://export.arxiv.org/rss/cs.AI"),
    ];
    let mut added = 0usize;
    for (name, url) in defaults {
        let res = sqlx::query(
            "INSERT OR IGNORE INTO sources (name, url, kind, enabled)
             VALUES (?1, ?2, 'feed', 1)",
        )
        .bind(name).bind(url)
        .execute(pool).await?;
        if res.rows_affected() > 0 {
            added += 1;
            tracing::info!("fetcher: added new source {:?}", name);
        }
    }
    if added > 0 {
        tracing::info!("fetcher: ensured default sources ({} new)", added);
    }
    // Disable (do not delete) sources we've retired so old items survive
    // for history but no new pulls happen. Reddit's RSS endpoint reliably
    // 403s against any non-OAuth client now.
    let _ = sqlx::query("UPDATE sources SET enabled = 0 WHERE name = 'r/LocalLLaMA'")
        .execute(pool).await;
    Ok(())
}

async fn list_enabled_sources(pool: &SqlitePool) -> Result<Vec<SourceRow>> {
    let rows: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT id, name, url FROM sources WHERE enabled = 1"
    ).fetch_all(pool).await?;
    Ok(rows.into_iter().map(|(id, name, url)| SourceRow { id, name, url }).collect())
}

async fn fetch_source(http: &reqwest::Client, src: &SourceRow) -> Result<Vec<ParsedItem>> {
    let bytes = http.get(&src.url).send().await?.error_for_status()?.bytes().await?;
    let feed = feed_rs::parser::parse(&bytes[..]).context("parsing feed")?;
    let mut out = Vec::with_capacity(feed.entries.len());
    let now_iso = Utc::now().to_rfc3339();
    for entry in feed.entries {
        let feed_link = entry.links.first().map(|l| l.href.clone()).unwrap_or_default();
        if feed_link.is_empty() {
            continue;
        }
        let title = entry.title.map(|t| t.content).unwrap_or_else(|| "(untitled)".into());
        let raw_summary = entry.summary.map(|s| s.content);
        let summary = raw_summary.as_deref().map(sanitize_summary);

        // For curation sites that wrap an external article, we want clicks
        // to land on the discussion (HN, lobste.rs), not the underlying
        // article. Extract the discussion URL from the feed body where it
        // lives, store the original feed link as article_url.
        let (url, article_url) = pick_primary_url(&src.name, &feed_link, raw_summary.as_deref());

        let published = entry.published.or(entry.updated).unwrap_or_else(Utc::now);
        let id = hash_id(&src.id, &url);
        out.push(ParsedItem {
            id,
            source_id: src.id,
            url,
            article_url,
            title,
            summary,
            published_at: published.to_rfc3339(),
            published_date: published.format("%Y-%m-%d").to_string(),
        });
        let _ = now_iso; // currently used for fetched_at via DB CURRENT_TIMESTAMP
    }
    Ok(out)
}

/// Pick the URL clicks should land on (`url`) and the secondary article URL
/// to surface as a small chip (`article_url`).
///
/// - **Hacker News**: hnrss.org puts the comments link inline in the body
///   as "Comments URL: https://news.ycombinator.com/item?id=…". The feed's
///   own `<link>` is the *article* URL. We swap them — discussion becomes
///   primary, article becomes the chip.
/// - **lobste.rs**: their RSS body has "Comments: https://lobste.rs/s/…".
///   Same swap.
/// - **Other sources**: leave feed link as primary; no secondary chip.
fn pick_primary_url(source_name: &str, feed_link: &str, raw_summary: Option<&str>) -> (String, Option<String>) {
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

/// Find the first `http(s)://…` URL that appears *after* a labelled marker
/// (case-insensitive substring match). Robust against both raw HTML and
/// sanitized one-line summaries, since it doesn't depend on line breaks or
/// HTML tags. Trailing punctuation / quotes / brackets are stripped.
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

fn sanitize_summary(s: &str) -> String {
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
    // Truncate by char count, not byte index — multi-byte UTF-8 (smart
    // quotes, emoji) lands mid-character at a byte boundary and panics.
    if trimmed.chars().count() > 600 {
        let head: String = trimmed.chars().take(600).collect();
        format!("{}...", head)
    } else {
        trimmed
    }
}

fn hash_id(source_id: &i64, url: &str) -> String {
    let mut h = Sha256::new();
    h.update(source_id.to_le_bytes());
    h.update(url.as_bytes());
    hex::encode(&h.finalize()[..12])
}

async fn upsert_items(pool: &SqlitePool, items: &[ParsedItem]) -> Result<Vec<ParsedItem>> {
    let now = Utc::now();
    let now_iso = now.to_rfc3339();
    let fetch_date = now.format("%Y-%m-%d").to_string();
    let mut new = Vec::new();
    for it in items {
        let res = sqlx::query(
            r#"INSERT OR IGNORE INTO items
               (id, source_id, url, article_url, title, summary, published_at, published_date, fetched_at, fetch_date)
               VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"#,
        )
        .bind(&it.id)
        .bind(it.source_id)
        .bind(&it.url)
        .bind(&it.article_url)
        .bind(&it.title)
        .bind(&it.summary)
        .bind(&it.published_at)
        .bind(&it.published_date)
        .bind(&now_iso)
        .bind(&fetch_date)
        .execute(pool)
        .await?;
        if res.rows_affected() > 0 {
            new.push(it.clone());
        }
    }
    Ok(new)
}

/// Delete a batch of items by id. Used by the score-then-cap pipeline to
/// drop everything outside the top POST_SCORE_CAP of a source.
async fn delete_items(pool: &SqlitePool, ids: &[String]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    // Votes have FK to items.id; we don't keep votes for unranked items.
    // sqlx doesn't expand `IN (?,?,?)` from a slice — bind one at a time.
    let mut tx = pool.begin().await?;
    for id in ids {
        sqlx::query("DELETE FROM votes WHERE item_id = ?1").bind(id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM items WHERE id = ?1").bind(id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

async fn mark_source_ok(pool: &SqlitePool, id: i64) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query("UPDATE sources SET last_fetched_at = ?1, last_error = NULL WHERE id = ?2")
        .bind(now).bind(id).execute(pool).await?;
    Ok(())
}

async fn mark_source_error(pool: &SqlitePool, id: i64, err: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query("UPDATE sources SET last_fetched_at = ?1, last_error = ?2 WHERE id = ?3")
        .bind(now).bind(err).bind(id).execute(pool).await?;
    Ok(())
}

const SCORE_BATCH_SIZE: usize = 20;

/// Score every item in `items`, batching to keep each Claude call bounded.
/// Returns one ScoredItem per input item (or fewer if Claude drops some).
async fn score_all(workspace_root: &PathBuf, items: &[ParsedItem], user_name: &str) -> Result<Vec<ScoredItem>> {
    let mut out: Vec<ScoredItem> = Vec::with_capacity(items.len());
    for chunk in items.chunks(SCORE_BATCH_SIZE) {
        match score_notability(workspace_root, chunk, user_name).await {
            Ok(batch) => out.extend(batch),
            Err(e) => tracing::warn!("fetcher: score batch failed ({}): {}", chunk.len(), e),
        }
    }
    Ok(out)
}

async fn score_notability(workspace_root: &PathBuf, items: &[ParsedItem], user_name: &str) -> Result<Vec<ScoredItem>> {
    if items.is_empty() {
        return Ok(vec![]);
    }
    let payload: Vec<serde_json::Value> = items
        .iter()
        .map(|i| {
            serde_json::json!({
                "id": i.id,
                "title": i.title,
                "summary": i.summary.clone().unwrap_or_default(),
            })
        })
        .collect();

    // Base rubric ALWAYS applies. Learned preferences (from the
    // preference-learner reading recent up/downvotes) are additive
    // refinements on top — they never override the base. The user gets
    // up to 10 items/run (so up to 20/day across morning + evening cron),
    // staying inside their stated 10-15/day appetite.
    let base_rubric = "BASE RUBRIC (always applies):\n\
        \n\
        PRIORITIZE — score 0.7-1.0 for items that fit the user's day-to-day.\n\
        Three categories, all roughly equal weight:\n\
        \n\
          1. **Novel ways people are using AI / new AI tools.** New agent\n\
             workflow patterns, prompting techniques, novel coding-agent\n\
             setups, real shipped projects built with Claude Code / Cursor /\n\
             OpenCode / similar. Frontier-model releases ALSO count here\n\
             (GPT-N, Claude N+1, Gemini X, Llama N) — score them on merit\n\
             based on what they actually ship, not on hype. A solid model\n\
             release is high-signal; a re-announce of an old release is not.\n\
        \n\
          2. **Local / self-hostable AI we can actually benefit from.**\n\
             whisper.cpp, ollama, llama.cpp, local TTS / STT / embedding /\n\
             diffusion models, anything you can run on your own hardware.\n\
             The user already runs whisper.cpp daily — this category is\n\
             tools and models in that vein, NOT the latest GPT/Claude\n\
             release. (Frontier model releases belong in category 1, not\n\
             this one.)\n\
        \n\
          3. **General tech news.** Major outages, security incidents,\n\
             notable framework / IDE / runtime releases, deprecations,\n\
             regulatory shifts, significant changes to tools already in\n\
             daily use (Claude Code, Cursor, Obsidian, etc.).\n\
        \n\
        DEMOTE — score 0.0-0.4:\n\
          - Generic startup funding rounds, hiring drama, valuation news.\n\
          - Crypto / web3 / NFTs.\n\
          - Beginner tutorials, listicles, '10 things every X should know'.\n\
          - Hot takes / opinion pieces with no new info.\n\
          - Mainstream consumer tech (phones, watches, cars) unless\n\
            directly relevant to dev workflow.\n\
          - Aerospace / consumer space news.\n\
        \n\
        Aim for ~10 items per run scoring above 0.6 — the user has time\n\
        for that many. Be generous on borderline items in the three\n\
        priority categories above; precision matters less than making\n\
        sure they see what's relevant.";

    let learned_prefs = nucleus_core::memory::read("news_preferences").ok().unwrap_or_default();
    let learned_block = if learned_prefs.trim().is_empty() {
        String::new()
    } else {
        format!(
            "\n\nVOTE-LEARNED REFINEMENTS (additive — apply on top of the base rubric, don't replace it):\n{}",
            learned_prefs.trim()
        )
    };

    let prompt = format!(r#"You are scoring AI/tech news items for {user_name}, a senior software engineer.

{base_rubric}{learned_block}

For each item below, return a JSON array (no prose, no markdown fences) of objects
with shape: {{"id": "<id>", "score": <0..1 float>, "reason": "<one short clause>"}}.

Items:
{items}
"#,
        items = serde_json::to_string_pretty(&payload)?,
    );

    let mut session = Session::spawn(SpawnOptions {
        workspace_root: workspace_root.clone(),
        permission_mode: Some(PermissionMode::Auto),
        tmux_session: "nucleus-news-fetcher".into(),
        window_name: Some("score".into()),
        agent_label: Some("news-fetcher".into()),
        ..SpawnOptions::default()
    })
    .await
    .context("spawning claude session for scoring")?;

    let raw = session.ask(&prompt, AskOptions::default()).await?;
    let _ = session.close().await;

    let cleaned = raw.trim().trim_start_matches("```json").trim_start_matches("```").trim_end_matches("```").trim();
    let scored: Vec<ScoredItem> = serde_json::from_str(cleaned)
        .with_context(|| format!("parsing scored output: {}", cleaned))?;
    Ok(scored)
}

async fn save_score(pool: &SqlitePool, item_id: &str, score: f64, reason: &str) -> Result<()> {
    sqlx::query(
        "UPDATE items SET notable_score = ?1, notable_reason = ?2 WHERE id = ?3",
    )
    .bind(score).bind(reason).bind(item_id)
    .execute(pool).await?;
    Ok(())
}

async fn record_run_start(pool: &SqlitePool, run_id: &str) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query("INSERT INTO fetcher_runs (run_id, started_at, ok) VALUES (?1, ?2, 0)")
        .bind(run_id).bind(now).execute(pool).await?;
    Ok(())
}

async fn record_run_finish(pool: &SqlitePool, run_id: &str, items_new: usize, notable: usize) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "UPDATE fetcher_runs SET finished_at = ?1, items_new = ?2, items_notable = ?3, ok = 1 WHERE run_id = ?4",
    )
    .bind(now).bind(items_new as i64).bind(notable as i64).bind(run_id)
    .execute(pool).await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct NotableRow {
    id: String,
    title: String,
    url: String,
    notable_score: Option<f64>,
    notable_reason: Option<String>,
}

async fn post_top_notable(
    pool: &SqlitePool,
    channel_id: &str,
    n: usize,
    feed_url: Option<&str>,
) -> Result<usize> {
    // Top N notable items from this run that haven't been posted yet.
    let rows: Vec<NotableRow> = sqlx::query_as::<_, NotableRow>(
        r#"SELECT id, title, url, notable_score, notable_reason
           FROM items
           WHERE notable_score IS NOT NULL
             AND notable_score >= 0.6
             AND posted_to_discord = 0
           ORDER BY notable_score DESC, fetched_at DESC
           LIMIT ?1"#,
    )
    .bind(n as i64)
    .fetch_all(pool)
    .await?;

    if rows.is_empty() {
        return Ok(0);
    }

    let mut body = String::from("@here 📰 **Notable in news today:**\n");
    for r in &rows {
        body.push_str(&format!(
            "• **{}** — {}\n  {}\n",
            r.title.trim(),
            r.notable_reason.clone().unwrap_or_default(),
            r.url
        ));
    }
    if let Some(url) = feed_url {
        body.push_str(&format!("\n→ Full feed: {}", url.trim_end_matches('/')));
    }

    // send_announcement: suppresses URL embeds AND enables @here parsing.
    discord_sdk::send_announcement(channel_id, &body).await?;

    for r in &rows {
        sqlx::query("UPDATE items SET posted_to_discord = 1 WHERE id = ?1")
            .bind(&r.id).execute(pool).await?;
    }
    Ok(rows.len())
}
