//! Vault search index (ADR-035): SQLite FTS5 over the vault's notes.
//!
//! Core owns `memory/vault_index.db` (ADR-020): only this module writes it,
//! from whichever process calls it (the `vault-search` CLI, the dashboard's
//! search endpoint, `vault-check`). The index is derived data. Loss or
//! corruption is repaired by deleting the file; the next call rebuilds it.
//!
//! The design follows ADR-023's session index: incremental update by
//! (mtime, size) before every query, bm25 ranking, `snippet()` excerpts,
//! porter stemming. Differences: the unit is a note, not a turn; columns
//! are weighted (title > headings/tags > path > frontmatter > body);
//! diacritics are folded so `orcamento` finds `orçamento`; a change to the
//! exclusion rules rebuilds the index, so a newly excluded note disappears
//! at the next query.

use super::exclude::Exclusions;
use super::{bucket_of, display_name, note, scan};
use anyhow::{Context, Result};
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use std::collections::HashSet;
use std::path::Path;

pub const DB_PATH: &str = "memory/vault_index.db";

/// Snippet match markers (see [`VaultSearchHit::snippet`]).
pub const MATCH_START: char = '\u{2}';
pub const MATCH_END: char = '\u{3}';

/// bm25 column weights, in `notes_fts` column order:
/// title, headings, tags, path, meta, body.
const BM25_WEIGHTS: &str = "12.0, 4.0, 4.0, 3.0, 1.5, 1.0";

const MIGRATIONS: &[crate::migrate::Migration] = &[crate::migrate::Migration {
    version: 1,
    name: "adr035-vault-index",
    step: crate::migrate::Step::Sql(
        "CREATE TABLE IF NOT EXISTS notes (
            id          INTEGER PRIMARY KEY,
            path        TEXT NOT NULL UNIQUE,
            mtime       INTEGER NOT NULL,
            size        INTEGER NOT NULL,
            title       TEXT NOT NULL,
            bucket      TEXT NOT NULL,
            created     TEXT,
            source      TEXT,
            tags        TEXT NOT NULL,
            indexed_at  TEXT NOT NULL
        );
        CREATE VIRTUAL TABLE IF NOT EXISTS notes_fts USING fts5(
            title,
            headings,
            tags,
            path,
            meta,
            body,
            tokenize = 'porter unicode61 remove_diacritics 2'
        );
        CREATE TABLE IF NOT EXISTS index_meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
    ),
}];

pub async fn open(workspace_root: &Path) -> Result<SqlitePool> {
    open_at(&workspace_root.join(DB_PATH)).await
}

pub async fn open_at(db: &Path) -> Result<SqlitePool> {
    let pool = crate::db::open(db).await?;
    crate::migrate::migrate(&pool, MIGRATIONS).await?;
    Ok(pool)
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct UpdateStats {
    /// Markdown files seen after path exclusion.
    pub scanned: usize,
    pub indexed: usize,
    pub unchanged: usize,
    pub removed: usize,
    /// Notes left out because their text looks like credentials.
    pub content_excluded: usize,
    /// Files left out by a path glob (all types).
    pub path_excluded: usize,
    /// True when the exclusion rules changed and the index was rebuilt.
    pub rebuilt: bool,
}

/// Bring the index in line with the vault: (re)index notes whose (mtime,
/// size) changed, drop rows for notes that were deleted or are now
/// excluded. Unchanged notes are not read.
///
/// The whole update runs in one `BEGIN IMMEDIATE` transaction, so two
/// processes updating at once (a session's CLI call and the dashboard)
/// serialize on SQLite's write lock instead of racing on the same rows;
/// readers are not blocked (WAL).
pub async fn update(pool: &SqlitePool, vault: &Path, ex: &Exclusions) -> Result<UpdateStats> {
    let walk = scan::walk(vault, ex)?;
    let mut conn = pool.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    match update_locked(&mut conn, &walk, ex).await {
        Ok(stats) => {
            sqlx::query("COMMIT").execute(&mut *conn).await?;
            Ok(stats)
        }
        Err(e) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(e)
        }
    }
}

async fn update_locked(
    conn: &mut sqlx::SqliteConnection,
    walk: &scan::Walk,
    ex: &Exclusions,
) -> Result<UpdateStats> {
    let mut stats = UpdateStats { path_excluded: walk.excluded.len(), ..Default::default() };

    // Exclusion rules changed → rebuild from scratch.
    let fp: Option<String> =
        sqlx::query_scalar("SELECT value FROM index_meta WHERE key = 'exclusions'")
            .fetch_optional(&mut *conn)
            .await?;
    if fp.as_deref() != Some(ex.fingerprint()) {
        sqlx::query("DELETE FROM notes_fts").execute(&mut *conn).await?;
        sqlx::query("DELETE FROM notes").execute(&mut *conn).await?;
        sqlx::query(
            "INSERT INTO index_meta (key, value) VALUES ('exclusions', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        )
        .bind(ex.fingerprint())
        .execute(&mut *conn)
        .await?;
        stats.rebuilt = fp.is_some();
    }

    let existing: Vec<(i64, String, i64, i64)> =
        sqlx::query_as("SELECT id, path, mtime, size FROM notes").fetch_all(&mut *conn).await?;
    let known: std::collections::HashMap<String, (i64, i64, i64)> = existing
        .into_iter()
        .map(|(id, p, m, s)| (p, (id, m, s)))
        .collect();

    let mut present: HashSet<String> = HashSet::new();
    for f in walk.files.iter().filter(|f| f.is_markdown()) {
        stats.scanned += 1;
        if let Some(&(_, m, s)) = known.get(&f.rel) {
            if m == f.mtime && s == f.size as i64 {
                present.insert(f.rel.clone());
                stats.unchanged += 1;
                continue;
            }
        }
        let Ok(text) = std::fs::read_to_string(&f.abs) else {
            // Unreadable (permissions, invalid UTF-8): treated as absent.
            continue;
        };
        if let Some(&(id, _, _)) = known.get(&f.rel) {
            delete_row(conn, id).await?;
        }
        present.insert(f.rel.clone());
        if ex.content_excluded(&text) {
            stats.content_excluded += 1;
            continue;
        }
        let n = note::parse(&f.rel, &text);
        let tags = n.tags.join(" ");
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO notes (path, mtime, size, title, bucket, created, source, tags, indexed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) RETURNING id",
        )
        .bind(&f.rel)
        .bind(f.mtime)
        .bind(f.size as i64)
        .bind(&n.title)
        .bind(bucket_of(&f.rel))
        .bind(n.created())
        .bind(n.source())
        .bind(&tags)
        .bind(chrono::Utc::now().to_rfc3339())
        .fetch_one(&mut *conn)
        .await?;
        sqlx::query(
            "INSERT INTO notes_fts (rowid, title, headings, tags, path, meta, body)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )
        .bind(id)
        .bind(&n.title)
        .bind(n.headings.join("\n"))
        .bind(&tags)
        .bind(path_words(&f.rel))
        .bind(&n.meta_text)
        .bind(&n.body)
        .execute(&mut *conn)
        .await?;
        stats.indexed += 1;
    }

    // Deleted, renamed away, or newly excluded.
    for (path, (id, _, _)) in &known {
        if !present.contains(path) {
            delete_row(conn, *id).await?;
            stats.removed += 1;
        }
    }
    Ok(stats)
}

async fn delete_row(conn: &mut sqlx::SqliteConnection, id: i64) -> Result<()> {
    sqlx::query("DELETE FROM notes_fts WHERE rowid = ?1").bind(id).execute(&mut *conn).await?;
    sqlx::query("DELETE FROM notes WHERE id = ?1").bind(id).execute(&mut *conn).await?;
    Ok(())
}

/// The path as words (`3-Projects/Alpha/api-notes.md` → `3 Projects Alpha
/// api notes`), so folder names are searchable.
fn path_words(rel: &str) -> String {
    rel.trim_end_matches(".md")
        .split(|c: char| c == '/' || c == '-' || c == '_' || c == '.')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct VaultSearchHit {
    /// Vault-relative path.
    pub path: String,
    pub title: String,
    /// File name, with the parent folder for generic names (`Alpha/index.md`).
    pub display: String,
    /// Top-level PARA bucket; empty for root files.
    pub bucket: String,
    /// Frontmatter `created`, verbatim.
    pub created: Option<String>,
    pub source: Option<String>,
    /// Best-matching excerpt. Matched terms are wrapped in U+0002 … U+0003
    /// ([`MATCH_START`] / [`MATCH_END`]), characters that never occur in
    /// note text, so a client can highlight them without confusing them
    /// with the brackets of a `[[link]]`.
    pub snippet: String,
    /// Relevance, higher is better (negated bm25).
    pub score: f64,
}

#[derive(Debug, Clone, Default)]
pub struct SearchOpts<'a> {
    /// Path prefix: a bucket (`3-Projects`) or a folder (`3-Projects/Alpha`).
    pub bucket: Option<&'a str>,
    pub limit: i64,
}

#[derive(Debug, Clone, Serialize, ts_rs::TS)]
#[ts(export)]
pub struct VaultSearchResult {
    pub hits: Vec<VaultSearchHit>,
    /// `all` = every term matched; `any` = no note had all terms, so the
    /// results match at least one (fewer terms per note, lower confidence).
    pub mode: String,
}

/// Search the index. Plain text is split into terms that must all match
/// (each term quoted, so punctuation is safe). When nothing matches all
/// terms, the search falls back to notes matching any term. A query that
/// uses FTS5 syntax (`OR`, `NOT`, `"phrase"`, `prefix*`) is passed through
/// as written when it parses.
pub async fn search(
    pool: &SqlitePool,
    query: &str,
    opts: &SearchOpts<'_>,
    ex: &Exclusions,
) -> Result<VaultSearchResult> {
    let terms = query_terms(query);
    if terms.is_empty() {
        return Ok(VaultSearchResult { hits: vec![], mode: "all".into() });
    }
    let limit = if opts.limit <= 0 { 10 } else { opts.limit.min(200) };

    if looks_like_fts_syntax(query) {
        if let Ok(hits) = run_match(pool, query, opts.bucket, limit, ex).await {
            return Ok(VaultSearchResult { hits, mode: "all".into() });
        }
    }
    let all = terms.iter().map(|t| quote(t)).collect::<Vec<_>>().join(" AND ");
    let hits = run_match(pool, &all, opts.bucket, limit, ex).await?;
    if !hits.is_empty() || terms.len() == 1 {
        return Ok(VaultSearchResult { hits, mode: "all".into() });
    }
    let any = terms.iter().map(|t| quote(t)).collect::<Vec<_>>().join(" OR ");
    let hits = run_match(pool, &any, opts.bucket, limit, ex).await?;
    Ok(VaultSearchResult { hits, mode: "any".into() })
}

fn query_terms(q: &str) -> Vec<String> {
    q.split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .map(|t| t.trim_matches('\'').to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

fn quote(t: &str) -> String {
    format!("\"{}\"", t.replace('"', "\"\""))
}

fn looks_like_fts_syntax(q: &str) -> bool {
    q.contains('"')
        || q.contains('*')
        || q.split_whitespace().any(|w| matches!(w, "OR" | "AND" | "NOT" | "NEAR"))
}

async fn run_match(
    pool: &SqlitePool,
    fts: &str,
    bucket: Option<&str>,
    limit: i64,
    ex: &Exclusions,
) -> Result<Vec<VaultSearchHit>> {
    let prefix = bucket.map(|b| b.trim_matches('/').to_string()).filter(|b| !b.is_empty());
    let sql = format!(
        "SELECT n.path, n.title, n.bucket, n.created, n.source,
                snippet(notes_fts, -1, char(2), char(3), ' … ', 14) AS snip,
                bm25(notes_fts, {BM25_WEIGHTS}) AS score
           FROM notes_fts JOIN notes n ON n.id = notes_fts.rowid
          WHERE notes_fts MATCH ?1
            AND (?2 IS NULL OR n.path = ?2 OR n.path LIKE ?2 || '/%')
          ORDER BY score
          LIMIT ?3"
    );
    let rows = sqlx::query(&sql)
        .bind(fts)
        .bind(prefix)
        .bind(limit)
        .fetch_all(pool)
        .await
        .context("vault search query failed")?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let path: String = r.get("path");
            VaultSearchHit {
                display: display_name(&path),
                title: r.get("title"),
                bucket: r.get("bucket"),
                created: r.get("created"),
                source: r.get("source"),
                snippet: r.get::<String, _>("snip").replace('\n', " "),
                score: -r.get::<f64, _>("score"),
                path,
            }
        })
        // Defense in depth: a row can only exist for a non-excluded path,
        // but the rules are re-checked on the way out.
        .filter(|h| !ex.path_excluded(&h.path))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn ex() -> Exclusions {
        Exclusions::new(&["**/attachments/**".into()], "").unwrap()
    }

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    /// Synthetic vault: every name and body here is invented.
    fn fixture(root: &Path) {
        write(root, "0-Inbox/README.md", "# Inbox\nUnsorted captures.\n");
        write(
            root,
            "3-Projects/Alpha/index.md",
            "---\ncreated: 2026-01-02\nsource: manual\ntags: [rocket]\n---\n# Alpha\n\nHub for the rocket engine project. See [[engine-notes]].\n",
        );
        write(
            root,
            "3-Projects/Beta/index.md",
            "---\ncreated: 2026-02-03\nsource: manual\n---\n# Beta\n\nHub for the garden irrigation project.\n",
        );
        write(
            root,
            "3-Projects/Alpha/engine-notes.md",
            "---\ncreated: 2026-01-05\nsource: whatsapp-braindump\n---\n# Engine notes\n\n## Orçamento\nThe orçamento for the turbopump was approved.\n",
        );
        write(root, "6-Slipbox/deciding-under-uncertainty.md", "# Deciding\n\nWe decided to wait.\n");
        write(root, "4-Areas/Homelab/router.md", "# Router\n\nrouter login notes\n");
        write(root, "4-Areas/Home/wifi.md", "# Wifi\n\n- **Password:** correct-horse\n");
        write(root, "3-Projects/Alpha/attachments/spec.md", "# Spec\nrocket attachment\n");
        write(root, ".obsidian/workspace.md", "rocket");
    }

    /// Two writers (a CLI call and the dashboard) updating at once both
    /// succeed and leave one row per note.
    #[tokio::test]
    async fn concurrent_updates_serialize() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        fixture(&vault);
        let db = tmp.path().join("idx.db");
        let (a, b) = (open_at(&db).await.unwrap(), open_at(&db).await.unwrap());
        let ex = ex();
        let (ra, rb) = tokio::join!(update(&a, &vault, &ex), update(&b, &vault, &ex));
        let (ra, rb) = (ra.unwrap(), rb.unwrap());
        assert_eq!(ra.indexed + rb.indexed, 5);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notes").fetch_one(&a).await.unwrap();
        assert_eq!(n, 5);
    }

    #[tokio::test]
    async fn index_search_incremental_and_deletes() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        fixture(&vault);
        let pool = open_at(&tmp.path().join("idx.db")).await.unwrap();
        let ex = ex();

        let s = update(&pool, &vault, &ex).await.unwrap();
        assert_eq!(s.indexed, 5, "{s:?}");
        assert_eq!(s.content_excluded, 1);
        assert!(s.path_excluded >= 2); // homelab note + attachment
        let again = update(&pool, &vault, &ex).await.unwrap();
        assert_eq!((again.indexed, again.unchanged), (0, 5));

        let opts = SearchOpts { bucket: None, limit: 10 };
        // Title/tag weighting: the hub tagged `rocket` outranks the others.
        let r = search(&pool, "rocket", &opts, &ex).await.unwrap();
        assert_eq!(r.hits[0].path, "3-Projects/Alpha/index.md");
        assert_eq!(r.hits[0].display, "Alpha/index.md");
        assert_eq!(r.hits[0].title, "Alpha");
        assert!(r.hits.iter().all(|h| !h.path.contains("attachments")));

        // Diacritics folded, porter stemming.
        let r = search(&pool, "orcamento", &opts, &ex).await.unwrap();
        assert_eq!(r.hits[0].path, "3-Projects/Alpha/engine-notes.md");
        assert!(r.hits[0].snippet.contains(MATCH_START) && r.hits[0].snippet.contains(MATCH_END));
        let r = search(&pool, "decides", &opts, &ex).await.unwrap();
        assert_eq!(r.hits.len(), 1);

        // Credentials never come back: not by folder, not by content.
        for q in ["router login", "correct horse", "password", "wifi"] {
            let r = search(&pool, q, &opts, &ex).await.unwrap();
            assert!(r.hits.is_empty(), "{q} returned {:?}", r.hits);
        }

        // Bucket filter + fallback to any-term.
        let r = search(&pool, "project hub", &SearchOpts { bucket: Some("3-Projects/Beta"), limit: 5 }, &ex)
            .await
            .unwrap();
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].bucket, "3-Projects");
        let r = search(&pool, "turbopump irrigation", &opts, &ex).await.unwrap();
        assert_eq!(r.mode, "any");
        assert_eq!(r.hits.len(), 2);

        // Punctuation is safe; FTS syntax passes through.
        assert!(search(&pool, "engine-notes: (x", &opts, &ex).await.is_ok());
        let r = search(&pool, "turbopump OR irrigation", &opts, &ex).await.unwrap();
        assert_eq!((r.mode.as_str(), r.hits.len()), ("all", 2));

        // Edit → reindexed; delete → removed; credential added → removed.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        write(&vault, "6-Slipbox/deciding-under-uncertainty.md", "# Deciding\n\nNew body about kayaks.\n");
        fs::remove_file(vault.join("3-Projects/Beta/index.md")).unwrap();
        write(&vault, "3-Projects/Alpha/engine-notes.md", "# Engine\n\napi_key = abc123\n");
        let s = update(&pool, &vault, &ex).await.unwrap();
        assert_eq!((s.indexed, s.removed, s.content_excluded), (1, 1, 2), "{s:?}");
        assert_eq!(search(&pool, "kayaks", &opts, &ex).await.unwrap().hits.len(), 1);
        assert!(search(&pool, "irrigation", &opts, &ex).await.unwrap().hits.is_empty());
        assert!(search(&pool, "engine", &opts, &ex).await.unwrap().hits.iter().all(|h| h.path != "3-Projects/Alpha/engine-notes.md"));

        // Changing the exclusion rules rebuilds and drops the newly excluded.
        let ex2 = Exclusions::new(&["6-Slipbox/**".into()], "").unwrap();
        let s = update(&pool, &vault, &ex2).await.unwrap();
        assert!(s.rebuilt);
        assert!(search(&pool, "kayaks", &opts, &ex2).await.unwrap().hits.is_empty());
    }
}
