//! Vault search index (ADR-035): SQLite FTS5 over the vault's notes.
//!
//! Writer (ADR-020 DB ownership): the `nucleus vault-search` command is the
//! only program that writes `memory/vault_index.db`, through [`Writer`].
//! Other processes read it: the dashboard runs `nucleus vault-search
//! --reindex` as a subprocess before a search and then opens the file
//! read-only ([`open_read_only`]). Invocations of the command that run at
//! the same time serialize on an advisory lock (`vault_index.lock`) and,
//! inside it, on SQLite's write lock. Each update reads the exclusion rules
//! from nucleus.toml after it holds the lock, so an update always applies
//! the rules that are current when it runs: no caller can write the index
//! under rules it loaded earlier. The index is derived data. Loss or
//! corruption is repaired by deleting the file; the next call rebuilds it.
//!
//! The design follows ADR-023's session index: incremental update by file
//! identity (device, inode, size, nanosecond mtime and ctime) before every
//! query, bm25 ranking, `snippet()` excerpts,
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
use std::path::{Path, PathBuf};

pub const DB_PATH: &str = "memory/vault_index.db";
/// Advisory lock that serializes writers across processes.
pub const LOCK_PATH: &str = "memory/vault_index.lock";

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
},
// Full file identity ([`scan::IndexIdentity`]). Existing rows get zeros,
// which match no real file, so every note is re-read once.
crate::migrate::Migration {
    version: 2,
    name: "adr035-vault-index-file-identity",
    step: crate::migrate::Step::Sql(
        "ALTER TABLE notes ADD COLUMN dev INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE notes ADD COLUMN ino INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE notes ADD COLUMN mtime_ns INTEGER NOT NULL DEFAULT 0;
         ALTER TABLE notes ADD COLUMN ctime_ns INTEGER NOT NULL DEFAULT 0",
    ),
}];

/// Open the index for reading. `Ok(None)` when it does not exist yet (no
/// writer has run). Never creates or migrates the file.
pub async fn open_read_only(workspace_root: &Path) -> Result<Option<SqlitePool>> {
    open_read_only_at(&workspace_root.join(DB_PATH)).await
}

pub async fn open_read_only_at(db: &Path) -> Result<Option<SqlitePool>> {
    if !db.exists() {
        return Ok(None);
    }
    Ok(Some(crate::db::open_read_only(db).await?))
}

/// The one write path into `vault_index.db`. See the module docs.
pub struct Writer {
    pool: SqlitePool,
    lock_path: PathBuf,
    workspace_root: PathBuf,
}

impl Writer {
    pub async fn open(workspace_root: &Path) -> Result<Self> {
        Self::open_at(workspace_root, &workspace_root.join(DB_PATH), &workspace_root.join(LOCK_PATH)).await
    }

    /// Explicit DB and lock paths (tests). The exclusion rules still come
    /// from `<workspace_root>/nucleus.toml`.
    pub async fn open_at(workspace_root: &Path, db: &Path, lock_path: &Path) -> Result<Self> {
        let pool = crate::db::open(db).await?;
        crate::migrate::migrate(&pool, MIGRATIONS).await?;
        Ok(Self { pool, lock_path: lock_path.to_path_buf(), workspace_root: workspace_root.to_path_buf() })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Bring the index in line with the vault and with the exclusion rules
    /// in nucleus.toml as they are now. Returns the stats and the rules the
    /// update applied, which a search in the same process should use.
    pub async fn update(&self, vault: &Path) -> Result<(UpdateStats, Exclusions)> {
        let lock_path = self.lock_path.clone();
        let _lock = tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
            let f = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&lock_path)
                .with_context(|| format!("opening {}", lock_path.display()))?;
            f.lock().with_context(|| format!("locking {}", lock_path.display()))?;
            Ok(f)
        })
        .await
        .context("vault index lock task")??;
        // Rules are read only now, under the lock.
        let ex = Exclusions::load(&self.workspace_root)?;
        let stats = update(&self.pool, vault, &ex).await?;
        Ok((stats, ex))
    }
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
    /// Notes larger than [`super::MAX_NOTE_BYTES`], not read or indexed.
    pub oversized: usize,
    /// True when the exclusion rules changed and the index was rebuilt.
    pub rebuilt: bool,
}

/// Bring the index in line with the vault: (re)index notes whose file
/// identity ([`scan::IndexIdentity`]: device, inode, size, nanosecond mtime
/// and ctime) changed, drop rows for notes that were deleted or are now
/// excluded. Unchanged notes are not read.
///
/// The whole update runs in one `BEGIN IMMEDIATE` transaction; readers are
/// not blocked (WAL). Private: [`Writer::update`] is the entry point, so
/// every update runs under the lock with the rules read inside it.
async fn update(pool: &SqlitePool, vault: &Path, ex: &Exclusions) -> Result<UpdateStats> {
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

    let existing: Vec<(i64, String, i64, i64, i64, i64, i64)> =
        sqlx::query_as("SELECT id, path, dev, ino, size, mtime_ns, ctime_ns FROM notes")
            .fetch_all(&mut *conn)
            .await?;
    let known: std::collections::HashMap<String, (i64, scan::IndexIdentity)> = existing
        .into_iter()
        .map(|(id, p, dev, ino, size, mtime_ns, ctime_ns)| {
            (p, (id, scan::IndexIdentity { dev, ino, size, mtime_ns, ctime_ns }))
        })
        .collect();

    let mut present: HashSet<String> = HashSet::new();
    for f in walk.files.iter().filter(|f| f.is_markdown()) {
        stats.scanned += 1;
        if let Some(&(_, stored)) = known.get(&f.rel) {
            if stored == f.identity() {
                present.insert(f.rel.clone());
                stats.unchanged += 1;
                continue;
            }
        }
        if f.size > super::MAX_NOTE_BYTES {
            stats.oversized += 1;
            continue;
        }
        let text = match walk.read_note(f) {
            Ok(Some(t)) => t,
            Ok(None) => {
                stats.oversized += 1;
                continue;
            }
            // Unreadable (permissions, invalid UTF-8): treated as absent.
            Err(_) => continue,
        };
        if let Some(&(id, _)) = known.get(&f.rel) {
            delete_row(conn, id).await?;
        }
        present.insert(f.rel.clone());
        if ex.content_excluded(&text) {
            stats.content_excluded += 1;
            continue;
        }
        let n = note::parse(&f.rel, &text);
        let tags = n.tags.join(" ");
        let ident = f.identity();
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO notes (path, mtime, size, title, bucket, created, source, tags, indexed_at,
                                dev, ino, mtime_ns, ctime_ns)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13) RETURNING id",
        )
        .bind(&f.rel)
        .bind(f.mtime)
        .bind(ident.size)
        .bind(&n.title)
        .bind(bucket_of(&f.rel))
        .bind(n.created())
        .bind(n.source())
        .bind(&tags)
        .bind(chrono::Utc::now().to_rfc3339())
        .bind(ident.dev)
        .bind(ident.ino)
        .bind(ident.mtime_ns)
        .bind(ident.ctime_ns)
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
        .bind(n.body(&text))
        .execute(&mut *conn)
        .await?;
        stats.indexed += 1;
    }

    // Deleted, renamed away, or newly excluded.
    for (path, (id, _)) in &known {
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

/// Escape a backslash, `%` and `_` for a `LIKE` pattern with a backslash
/// escape character.
fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

async fn run_match(
    pool: &SqlitePool,
    fts: &str,
    bucket: Option<&str>,
    limit: i64,
    ex: &Exclusions,
) -> Result<Vec<VaultSearchHit>> {
    let prefix = bucket.map(|b| b.trim_matches('/').to_string()).filter(|b| !b.is_empty());
    // The prefix is literal: `%` and `_` in it match only themselves.
    let like = prefix.as_deref().map(|p| format!("{}/%", like_escape(p)));
    let sql = format!(
        "SELECT n.path, n.title, n.bucket, n.created, n.source,
                snippet(notes_fts, -1, char(2), char(3), ' … ', 14) AS snip,
                bm25(notes_fts, {BM25_WEIGHTS}) AS score
           FROM notes_fts JOIN notes n ON n.id = notes_fts.rowid
          WHERE notes_fts MATCH ?1
            AND (?2 IS NULL OR n.path = ?2 OR n.path LIKE ?4 ESCAPE '\\')
          ORDER BY score
          LIMIT ?3"
    );
    let rows = sqlx::query(&sql)
        .bind(fts)
        .bind(prefix)
        .bind(limit)
        .bind(like)
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

    fn write(root: &Path, rel: &str, text: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, text).unwrap();
    }

    /// Workspace whose nucleus.toml adds `extra` to the exclusion floor.
    fn set_rules(ws: &Path, extra: &[&str]) {
        let list = extra.iter().map(|g| format!("{g:?}")).collect::<Vec<_>>().join(", ");
        write(ws, "nucleus.toml", &format!("[vault_search]\nexclude = [{list}]\n"));
    }

    async fn writer(tmp: &Path) -> Writer {
        Writer::open_at(&tmp.join("ws"), &tmp.join("idx.db"), &tmp.join("idx.lock")).await.unwrap()
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

    /// Two writers (two `vault-search` processes) updating at once both
    /// succeed and leave one row per note.
    #[tokio::test]
    async fn concurrent_updates_serialize() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        fixture(&vault);
        set_rules(&tmp.path().join("ws"), &["**/attachments/**"]);
        let (a, b) = (writer(tmp.path()).await, writer(tmp.path()).await);
        let (ra, rb) = tokio::join!(a.update(&vault), b.update(&vault));
        let (ra, rb) = (ra.unwrap().0, rb.unwrap().0);
        assert_eq!(ra.indexed + rb.indexed, 5);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notes").fetch_one(a.pool()).await.unwrap();
        assert_eq!(n, 5);
    }

    /// H2 regression: two long-lived callers, one started before the
    /// operator added an exclusion and one after. Neither can put the newly
    /// excluded note back, because each update reads the rules under the
    /// lock instead of using rules captured at start-up.
    #[tokio::test]
    async fn an_older_caller_cannot_restore_stale_rules() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let vault = tmp.path().join("vault");
        fixture(&vault);
        set_rules(&ws, &["**/attachments/**"]);
        let old_caller = writer(tmp.path()).await;
        let (_, old_rules) = old_caller.update(&vault).await.unwrap();
        let opts = SearchOpts { bucket: None, limit: 10 };
        assert_eq!(search(old_caller.pool(), "kayaks OR decided", &opts, &old_rules).await.unwrap().hits.len(), 1);

        // The operator excludes 6-Slipbox; a new caller rebuilds.
        set_rules(&ws, &["**/attachments/**", "6-Slipbox/**"]);
        let new_caller = writer(tmp.path()).await;
        let (s, new_rules) = new_caller.update(&vault).await.unwrap();
        assert!(s.rebuilt);
        assert_ne!(old_rules.fingerprint(), new_rules.fingerprint());

        // The old caller updates again, alone and concurrently with the new one.
        let (ra, rb) = tokio::join!(old_caller.update(&vault), new_caller.update(&vault));
        let (ra, rb) = (ra.unwrap(), rb.unwrap());
        assert!(!ra.0.rebuilt && !rb.0.rebuilt, "no rebuild back to the old rules");
        assert_eq!(ra.1.fingerprint(), new_rules.fingerprint());
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM notes WHERE path LIKE '6-Slipbox/%'")
            .fetch_one(old_caller.pool())
            .await
            .unwrap();
        assert_eq!(n, 0);
        let fp: String = sqlx::query_scalar("SELECT value FROM index_meta WHERE key = 'exclusions'")
            .fetch_one(old_caller.pool())
            .await
            .unwrap();
        assert_eq!(fp, new_rules.fingerprint());
    }

    #[tokio::test]
    async fn index_search_incremental_and_deletes() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let vault = tmp.path().join("vault");
        fixture(&vault);
        set_rules(&ws, &["**/attachments/**"]);
        let w = writer(tmp.path()).await;
        let pool = w.pool();

        let (s, ex) = w.update(&vault).await.unwrap();
        assert_eq!(s.indexed, 5, "{s:?}");
        assert_eq!(s.content_excluded, 1);
        assert!(s.path_excluded >= 2); // homelab note + attachment
        let (again, _) = w.update(&vault).await.unwrap();
        assert_eq!((again.indexed, again.unchanged), (0, 5));

        let opts = SearchOpts { bucket: None, limit: 10 };
        // Title/tag weighting: the hub tagged `rocket` outranks the others.
        let r = search(pool, "rocket", &opts, &ex).await.unwrap();
        assert_eq!(r.hits[0].path, "3-Projects/Alpha/index.md");
        assert_eq!(r.hits[0].display, "Alpha/index.md");
        assert_eq!(r.hits[0].title, "Alpha");
        assert!(r.hits.iter().all(|h| !h.path.contains("attachments")));

        // Diacritics folded, porter stemming.
        let r = search(pool, "orcamento", &opts, &ex).await.unwrap();
        assert_eq!(r.hits[0].path, "3-Projects/Alpha/engine-notes.md");
        assert!(r.hits[0].snippet.contains(MATCH_START) && r.hits[0].snippet.contains(MATCH_END));
        let r = search(pool, "decides", &opts, &ex).await.unwrap();
        assert_eq!(r.hits.len(), 1);

        // Credentials never come back: not by folder, not by content.
        for q in ["router login", "correct horse", "password", "wifi"] {
            let r = search(pool, q, &opts, &ex).await.unwrap();
            assert!(r.hits.is_empty(), "{q} returned {:?}", r.hits);
        }

        // Bucket filter + fallback to any-term.
        let r = search(pool, "project hub", &SearchOpts { bucket: Some("3-Projects/Beta"), limit: 5 }, &ex)
            .await
            .unwrap();
        assert_eq!(r.hits.len(), 1);
        assert_eq!(r.hits[0].bucket, "3-Projects");
        let r = search(pool, "turbopump irrigation", &opts, &ex).await.unwrap();
        assert_eq!(r.mode, "any");
        assert_eq!(r.hits.len(), 2);

        // Punctuation is safe; FTS syntax passes through.
        assert!(search(pool, "engine-notes: (x", &opts, &ex).await.is_ok());
        let r = search(pool, "turbopump OR irrigation", &opts, &ex).await.unwrap();
        assert_eq!((r.mode.as_str(), r.hits.len()), ("all", 2));

        // Edit → reindexed; delete → removed; credential added → removed.
        write(&vault, "6-Slipbox/deciding-under-uncertainty.md", "# Deciding\n\nNew body about kayaks.\n");
        fs::remove_file(vault.join("3-Projects/Beta/index.md")).unwrap();
        write(&vault, "3-Projects/Alpha/engine-notes.md", "# Engine\n\napi_key = abc123\n");
        let (s, _) = w.update(&vault).await.unwrap();
        assert_eq!((s.indexed, s.removed, s.content_excluded), (1, 1, 2), "{s:?}");
        assert_eq!(search(pool, "kayaks", &opts, &ex).await.unwrap().hits.len(), 1);
        assert!(search(pool, "irrigation", &opts, &ex).await.unwrap().hits.is_empty());
        assert!(search(pool, "engine", &opts, &ex).await.unwrap().hits.iter().all(|h| h.path != "3-Projects/Alpha/engine-notes.md"));

        // Changing the exclusion rules rebuilds and drops the newly excluded.
        set_rules(&ws, &["6-Slipbox/**"]);
        let (s, ex2) = w.update(&vault).await.unwrap();
        assert!(s.rebuilt);
        assert!(search(pool, "kayaks", &opts, &ex2).await.unwrap().hits.is_empty());
    }

    /// Round 3, item 3: an edit in the same second that keeps the size is
    /// reindexed (nanosecond mtime and ctime), and so is an edit whose
    /// mtime a program set back (ctime), and a file replaced by another
    /// with the same size and mtime (inode).
    #[tokio::test]
    async fn same_second_same_size_edits_are_reindexed() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        write(&vault, "0-Inbox/n.md", "# N\nalpha\n");
        set_rules(&tmp.path().join("ws"), &[]);
        let w = writer(tmp.path()).await;
        let (_, ex) = w.update(&vault).await.unwrap();
        let hits = |q: &'static str| {
            let (pool, ex) = (w.pool().clone(), ex.clone());
            async move { search(&pool, q, &SearchOpts { bucket: None, limit: 10 }, &ex).await.unwrap().hits.len() }
        };
        assert_eq!(hits("alpha").await, 1);

        // Same length, immediately (same second), no sleep.
        let p = vault.join("0-Inbox/n.md");
        fs::write(&p, "# N\nbravo\n").unwrap();
        let (s, _) = w.update(&vault).await.unwrap();
        assert_eq!(s.indexed, 1, "{s:?}");
        assert_eq!((hits("alpha").await, hits("bravo").await), (0, 1));

        // Same length and the old mtime restored: ctime still differs.
        let mtime = fs::metadata(&p).unwrap().modified().unwrap();
        fs::write(&p, "# N\ncharl\n").unwrap();
        fs::File::options().write(true).open(&p).unwrap().set_modified(mtime).unwrap();
        let (s, _) = w.update(&vault).await.unwrap();
        assert_eq!(s.indexed, 1, "{s:?}");
        assert_eq!(hits("charl").await, 1);

        // Replaced by a different file (new inode) with the same size.
        let other = vault.join("0-Inbox/other.tmp");
        fs::write(&other, "# N\ndelta\n").unwrap();
        fs::rename(&other, &p).unwrap();
        let (s, _) = w.update(&vault).await.unwrap();
        assert_eq!(s.indexed, 1, "{s:?}");
        assert_eq!(hits("delta").await, 1);
    }

    /// `%` and `_` in a bucket filter match only themselves.
    #[tokio::test]
    async fn bucket_filter_is_literal() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        write(&vault, "3-Projects/Alpha/a.md", "# A\nrocket\n");
        write(&vault, "3_Projects/b.md", "# B\nrocket\n");
        write(&vault, "30-Other/c.md", "# C\nrocket\n");
        set_rules(&tmp.path().join("ws"), &[]);
        let w = writer(tmp.path()).await;
        let (_, ex) = w.update(&vault).await.unwrap();
        let hits = |b: &'static str| {
            let pool = w.pool().clone();
            let ex = ex.clone();
            async move {
                let r = search(&pool, "rocket", &SearchOpts { bucket: Some(b), limit: 10 }, &ex).await.unwrap();
                let mut p: Vec<String> = r.hits.into_iter().map(|h| h.path).collect();
                p.sort();
                p
            }
        };
        assert!(hits("3%").await.is_empty());
        assert!(hits("%").await.is_empty());
        assert_eq!(hits("3_Projects").await, vec!["3_Projects/b.md"]);
        assert_eq!(hits("3-Projects").await, vec!["3-Projects/Alpha/a.md"]);
    }

    /// A note over the size ceiling is counted and never read or indexed.
    #[tokio::test]
    async fn oversized_notes_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let vault = tmp.path().join("vault");
        write(&vault, "0-Inbox/small.md", "# Small\nrocket\n");
        let big = format!("# Big\nrocket\n{}", "x".repeat(super::super::MAX_NOTE_BYTES as usize));
        write(&vault, "0-Inbox/big.md", &big);
        set_rules(&tmp.path().join("ws"), &[]);
        let w = writer(tmp.path()).await;
        let (s, ex) = w.update(&vault).await.unwrap();
        assert_eq!((s.indexed, s.oversized), (1, 1), "{s:?}");
        let r = search(w.pool(), "rocket", &SearchOpts { bucket: None, limit: 10 }, &ex).await.unwrap();
        assert_eq!(r.hits.len(), 1);
    }
}
