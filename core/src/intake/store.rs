//! `memory/intake.db` (ADR-036): events, pipeline items, the item threads,
//! the stage log, and the collaborator cache.
//!
//! Every write goes through this module, inside the `nucleus` binary. A
//! stage change is one `BEGIN IMMEDIATE` transaction whose `WHERE` clause
//! re-checks the stage it moves from ([`advance`]), so two ticks (or a tick
//! and the dashboard) can never both move the same item.

use super::event::{Event, NewEvent};
use super::stage::{transition, Stage, StageEvent};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use sqlx::{Row, SqlitePool};
use std::path::Path;

const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS events (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    source        TEXT NOT NULL,
    external_id   TEXT NOT NULL,
    project       TEXT,
    kind          TEXT NOT NULL,
    title         TEXT NOT NULL,
    body          TEXT NOT NULL,
    author        TEXT,
    labels_json   TEXT NOT NULL,
    url           TEXT,
    state         TEXT NOT NULL,
    created_at    TEXT,
    updated_at    TEXT,
    raw_json      TEXT NOT NULL,
    accepted      INTEGER NOT NULL,
    first_seen_at TEXT NOT NULL,
    last_seen_at  TEXT NOT NULL,
    UNIQUE (source, external_id)
);
CREATE TABLE IF NOT EXISTS items (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id           INTEGER NOT NULL UNIQUE REFERENCES events(id),
    repo               TEXT NOT NULL,
    title              TEXT NOT NULL,
    stage              TEXT NOT NULL,
    failed_stage       TEXT,
    error              TEXT,
    classification     TEXT,
    eval_json          TEXT,
    plan_draft         TEXT,
    plan_version       INTEGER NOT NULL DEFAULT 0,
    approved_plan      TEXT,
    approved_version   INTEGER,
    approved_at        TEXT,
    approved_via       TEXT,
    branch             TEXT,
    worktree           TEXT,
    base_ref           TEXT,
    impl_summary       TEXT,
    tests_status       TEXT,
    tests_output       TEXT,
    pr_url             TEXT,
    comment_draft      TEXT,
    comment_state      TEXT NOT NULL DEFAULT 'none',
    comment_url        TEXT,
    surface            TEXT NOT NULL DEFAULT 'none',
    group_requested_at TEXT,
    group_jid          TEXT,
    group_closed_at    TEXT,
    current_task_id    TEXT,
    last_task_id       TEXT,
    step_errors        INTEGER NOT NULL DEFAULT 0,
    created_at         TEXT NOT NULL,
    updated_at         TEXT NOT NULL,
    closed_at          TEXT
);
CREATE INDEX IF NOT EXISTS idx_items_stage ON items(stage, id);
CREATE TABLE IF NOT EXISTS item_tasks (
    item_id    INTEGER NOT NULL REFERENCES items(id),
    task_id    TEXT NOT NULL,
    stage      TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (item_id, task_id)
);
CREATE TABLE IF NOT EXISTS item_messages (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id       INTEGER NOT NULL REFERENCES items(id),
    at            TEXT NOT NULL,
    author        TEXT NOT NULL,
    via           TEXT NOT NULL,
    body          TEXT NOT NULL,
    external_ref  TEXT UNIQUE,
    pending_agent INTEGER NOT NULL DEFAULT 0,
    read_by_task  TEXT,
    wa_state      TEXT,
    wa_outbound   INTEGER
);
CREATE INDEX IF NOT EXISTS idx_item_messages_item ON item_messages(item_id, id);
CREATE TABLE IF NOT EXISTS item_transitions (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id    INTEGER NOT NULL REFERENCES items(id),
    at         TEXT NOT NULL,
    from_stage TEXT,
    to_stage   TEXT NOT NULL,
    reason     TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS collaborators (
    repo       TEXT NOT NULL,
    login      TEXT NOT NULL,
    trusted    INTEGER NOT NULL,
    checked_at TEXT NOT NULL,
    PRIMARY KEY (repo, login)
);
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
)";

/// v2: the commit Nucleus collected from the item's clone and pushes.
const SCHEMA_V2: &str = "ALTER TABLE items ADD COLUMN head_sha TEXT";

/// v3 (finding: untrusted edits reach an acting session): an item is bound
/// to the revision of its event at the moment the gate was satisfied, and
/// to the gate event itself. `items` is rebuilt without `UNIQUE(event_id)`:
/// an event gets a new item each time its gate opens again (the label is
/// re-added, the issue reopened), and at most one open item at a time.
/// `item_comments` binds an item to every trusted comment it used. Events
/// record when their content last changed and when the gate was checked.
const V3_ITEM_COLUMNS: &str = "id, event_id, repo, title, stage, failed_stage, error, classification, eval_json, \
    plan_draft, plan_version, approved_plan, approved_version, approved_at, approved_via, branch, worktree, \
    base_ref, impl_summary, tests_status, tests_output, pr_url, comment_draft, comment_state, comment_url, surface, \
    group_requested_at, group_jid, group_closed_at, current_task_id, last_task_id, step_errors, created_at, \
    updated_at, closed_at, head_sha";

const SCHEMA_V3: &[&str] = &[
    "CREATE TABLE items_v3 (
        id                 INTEGER PRIMARY KEY AUTOINCREMENT,
        event_id           INTEGER NOT NULL REFERENCES events(id),
        repo               TEXT NOT NULL,
        title              TEXT NOT NULL,
        stage              TEXT NOT NULL,
        failed_stage       TEXT,
        error              TEXT,
        classification     TEXT,
        eval_json          TEXT,
        plan_draft         TEXT,
        plan_version       INTEGER NOT NULL DEFAULT 0,
        approved_plan      TEXT,
        approved_version   INTEGER,
        approved_at        TEXT,
        approved_via       TEXT,
        branch             TEXT,
        worktree           TEXT,
        base_ref           TEXT,
        impl_summary       TEXT,
        tests_status       TEXT,
        tests_output       TEXT,
        pr_url             TEXT,
        comment_draft      TEXT,
        comment_state      TEXT NOT NULL DEFAULT 'none',
        comment_url        TEXT,
        surface            TEXT NOT NULL DEFAULT 'none',
        group_requested_at TEXT,
        group_jid          TEXT,
        group_closed_at    TEXT,
        current_task_id    TEXT,
        last_task_id       TEXT,
        step_errors        INTEGER NOT NULL DEFAULT 0,
        created_at         TEXT NOT NULL,
        updated_at         TEXT NOT NULL,
        closed_at          TEXT,
        head_sha           TEXT,
        rev_title          TEXT,
        rev_body           TEXT,
        revision_hash      TEXT,
        gate_event_id      TEXT,
        label_event_id     TEXT,
        gate_actor         TEXT,
        gate_at            TEXT,
        stale_reason       TEXT
    )",
    "INSERT INTO items_v3 ({cols}) SELECT {cols} FROM items",
    "DROP TABLE items",
    "ALTER TABLE items_v3 RENAME TO items",
    "CREATE INDEX idx_items_stage ON items(stage, id)",
    "CREATE UNIQUE INDEX idx_items_open_event ON items(event_id) WHERE stage NOT IN ('closed', 'cancelled', 'stale')",
    "CREATE UNIQUE INDEX idx_items_gate ON items(event_id, gate_event_id) WHERE gate_event_id IS NOT NULL",
    "CREATE TABLE item_comments (
        item_id     INTEGER NOT NULL REFERENCES items(id),
        comment_id  TEXT NOT NULL,
        author      TEXT NOT NULL,
        body_hash   TEXT NOT NULL,
        recorded_at TEXT NOT NULL,
        PRIMARY KEY (item_id, comment_id)
    )",
    "ALTER TABLE events ADD COLUMN changed_at TEXT",
    "ALTER TABLE events ADD COLUMN gate_checked_at TEXT",
    "ALTER TABLE events ADD COLUMN gate_note TEXT",
    "UPDATE events SET changed_at = last_seen_at",
];

/// Runs [`SCHEMA_V3`] with foreign keys off (the documented SQLite
/// procedure for rebuilding a table other tables reference), in one
/// transaction, then checks every foreign key before committing.
fn migrate_v3(pool: &SqlitePool) -> futures::future::BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        let mut conn = pool.acquire().await?;
        sqlx::query("PRAGMA foreign_keys = OFF").execute(&mut *conn).await?;
        let result = async {
            sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
            let done: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM pragma_table_info('items') WHERE name = 'revision_hash'")
                    .fetch_one(&mut *conn)
                    .await?;
            if done > 0 {
                sqlx::query("ROLLBACK").execute(&mut *conn).await?;
                return Ok(());
            }
            let applied = async {
                for stmt in SCHEMA_V3 {
                    let stmt = stmt.replace("{cols}", V3_ITEM_COLUMNS);
                    sqlx::query(&stmt).execute(&mut *conn).await.with_context(|| format!("intake.db v3: {stmt}"))?;
                }
                let broken: Vec<(String,)> =
                    sqlx::query_as("SELECT \"table\" FROM pragma_foreign_key_check").fetch_all(&mut *conn).await?;
                if !broken.is_empty() {
                    bail!("intake.db v3 left broken foreign keys in {broken:?}");
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            match applied {
                Ok(()) => {
                    sqlx::query("COMMIT").execute(&mut *conn).await?;
                    Ok(())
                }
                Err(e) => {
                    let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
                    Err(e)
                }
            }
        }
        .await;
        sqlx::query("PRAGMA foreign_keys = ON").execute(&mut *conn).await?;
        result
    })
}

/// Open (creating and migrating) intake.db. Writers only.
pub async fn open(workspace_root: &Path) -> Result<SqlitePool> {
    let pool = crate::db::open(&workspace_root.join(super::INTAKE_DB_PATH)).await?;
    crate::migrate::migrate(
        &pool,
        &[
            crate::migrate::Migration { version: 1, name: "intake baseline", step: crate::migrate::Step::Sql(SCHEMA_V1) },
            crate::migrate::Migration { version: 2, name: "collected commit", step: crate::migrate::Step::Sql(SCHEMA_V2) },
            crate::migrate::Migration { version: 3, name: "revision binding", step: crate::migrate::Step::Rust(migrate_v3) },
        ],
    )
    .await
    .context("migrating intake.db")?;
    Ok(pool)
}

/// Open an existing intake.db read-only (dashboard reads).
pub async fn open_read_only(workspace_root: &Path) -> Result<SqlitePool> {
    crate::db::open_read_only(&workspace_root.join(super::INTAKE_DB_PATH)).await
}

// ── events ───────────────────────────────────────────────────────────────

const EVENT_COLUMNS: &str = "id, source, external_id, project, kind, title, body, author, labels_json, url, \
    state, created_at, updated_at, accepted, first_seen_at, last_seen_at, gate_note";

fn event_from_row(r: &sqlx::sqlite::SqliteRow) -> Result<Event> {
    Ok(Event {
        id: r.try_get("id")?,
        source: r.try_get("source")?,
        external_id: r.try_get("external_id")?,
        project: r.try_get("project")?,
        kind: r.try_get("kind")?,
        title: r.try_get("title")?,
        body: r.try_get("body")?,
        author: r.try_get("author")?,
        labels: serde_json::from_str(&r.try_get::<String, _>("labels_json")?).unwrap_or_default(),
        url: r.try_get("url")?,
        state: r.try_get("state")?,
        created_at: r.try_get("created_at")?,
        updated_at: r.try_get("updated_at")?,
        accepted: r.try_get::<i64, _>("accepted")? != 0,
        first_seen_at: r.try_get("first_seen_at")?,
        last_seen_at: r.try_get("last_seen_at")?,
        gate_note: r.try_get("gate_note")?,
    })
}

/// What [`upsert_event`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Upsert {
    Inserted,
    /// The record existed and a field changed.
    Updated,
    /// The record existed unchanged (the source reported it again).
    Unchanged,
}

fn validate_event(e: &NewEvent) -> Result<()> {
    if e.source.trim().is_empty() || e.external_id.trim().is_empty() {
        bail!("an event needs a source and an external id");
    }
    if !e.source.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("the source name {:?} may contain only letters, digits, - and _", e.source);
    }
    if e.title.trim().is_empty() {
        bail!("an event needs a title");
    }
    if !matches!(e.state.as_str(), "open" | "closed") {
        bail!("the event state must be open or closed, not {:?}", e.state);
    }
    Ok(())
}

/// Record an event, deduplicated by `(source, external_id)`: the first
/// report inserts it, a later report updates the fields the source may
/// change (title, body, labels, state, gate decision, raw). One transaction.
/// Also returns the event as it was before, for an update.
pub async fn upsert_event(pool: &SqlitePool, e: &NewEvent) -> Result<(Event, Upsert, Option<Event>)> {
    validate_event(e)?;
    let now = crate::timestamp::now();
    let labels = serde_json::to_string(&e.labels)?;
    let raw = serde_json::to_string(&e.raw)?;
    let norm = |t: &Option<String>| t.as_deref().map(crate::timestamp::to_sortable);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let existing = sqlx::query(&format!(
        "SELECT {EVENT_COLUMNS}, raw_json FROM events WHERE source = ?1 AND external_id = ?2"
    ))
    .bind(&e.source)
    .bind(&e.external_id)
    .fetch_optional(&mut *tx)
    .await?;
    let kind = match existing {
        None => {
            sqlx::query(
                "INSERT INTO events (source, external_id, project, kind, title, body, author, labels_json,
                                     url, state, created_at, updated_at, raw_json, accepted,
                                     first_seen_at, last_seen_at, changed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?15, ?15)",
            )
            .bind(&e.source)
            .bind(&e.external_id)
            .bind(&e.project)
            .bind(&e.kind)
            .bind(&e.title)
            .bind(&e.body)
            .bind(&e.author)
            .bind(&labels)
            .bind(&e.url)
            .bind(&e.state)
            .bind(norm(&e.created_at))
            .bind(norm(&e.updated_at))
            .bind(&raw)
            .bind(e.accepted as i64)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
            (Upsert::Inserted, None)
        }
        Some(row) => {
            let old = event_from_row(&row)?;
            let old_raw: String = row.try_get("raw_json")?;
            let changed = old.title != e.title
                || old.body != e.body
                || old.labels != e.labels
                || old.state != e.state
                || old.accepted != e.accepted
                || old.url != e.url
                || old.project != e.project
                || old_raw != raw;
            sqlx::query(
                "UPDATE events SET project = ?3, kind = ?4, title = ?5, body = ?6, author = ?7,
                        labels_json = ?8, url = ?9, state = ?10, updated_at = COALESCE(?11, updated_at),
                        raw_json = ?12, accepted = ?13, last_seen_at = ?14,
                        changed_at = CASE WHEN ?15 THEN ?14 ELSE changed_at END
                  WHERE source = ?1 AND external_id = ?2",
            )
            .bind(&e.source)
            .bind(&e.external_id)
            .bind(&e.project)
            .bind(&e.kind)
            .bind(&e.title)
            .bind(&e.body)
            .bind(&e.author)
            .bind(&labels)
            .bind(&e.url)
            .bind(&e.state)
            .bind(norm(&e.updated_at))
            .bind(&raw)
            .bind(e.accepted as i64)
            .bind(&now)
            .bind(changed)
            .execute(&mut *tx)
            .await?;
            (if changed { Upsert::Updated } else { Upsert::Unchanged }, Some(old))
        }
    };
    tx.commit().await?;
    let ev = event_by_key(pool, &e.source, &e.external_id).await?.context("event vanished")?;
    Ok((ev, kind.0, kind.1))
}

pub async fn event_by_key(pool: &SqlitePool, source: &str, external_id: &str) -> Result<Option<Event>> {
    let row = sqlx::query(&format!("SELECT {EVENT_COLUMNS} FROM events WHERE source = ?1 AND external_id = ?2"))
        .bind(source)
        .bind(external_id)
        .fetch_optional(pool)
        .await?;
    row.map(|r| event_from_row(&r)).transpose()
}

pub async fn event(pool: &SqlitePool, id: i64) -> Result<Event> {
    let row = sqlx::query(&format!("SELECT {EVENT_COLUMNS} FROM events WHERE id = ?1"))
        .bind(id)
        .fetch_one(pool)
        .await
        .with_context(|| format!("no event {id}"))?;
    event_from_row(&row)
}

/// Accepted, open events of `source` on one of `projects` that have no
/// open item and changed since their gate was last checked: the candidates
/// for a new item.
pub async fn gate_candidates(pool: &SqlitePool, source: &str, projects: &[String]) -> Result<Vec<Event>> {
    let rows = sqlx::query(&format!(
        "SELECT {EVENT_COLUMNS} FROM events e
          WHERE source = ?1 AND accepted = 1 AND state = 'open'
            AND (gate_checked_at IS NULL OR gate_checked_at < COALESCE(changed_at, last_seen_at))
            AND NOT EXISTS (SELECT 1 FROM items i WHERE i.event_id = e.id AND i.stage NOT IN ('closed','cancelled','stale'))
          ORDER BY id"
    ))
    .bind(source)
    .fetch_all(pool)
    .await?;
    let lower: Vec<String> = projects.iter().map(|p| p.to_lowercase()).collect();
    let mut out = Vec::new();
    for r in &rows {
        let e = event_from_row(r)?;
        if e.project.as_deref().map(|p| lower.contains(&p.to_lowercase())).unwrap_or(false) {
            out.push(e);
        }
    }
    Ok(out)
}

/// Set the stored title and body of event `id` to what the source returned
/// live (the gate check binds an item to that text; a later poll compares).
pub async fn set_event_content(pool: &SqlitePool, id: i64, title: &str, body: &str) -> Result<()> {
    sqlx::query("UPDATE events SET title = ?2, body = ?3 WHERE id = ?1")
        .bind(id)
        .bind(title)
        .bind(body)
        .execute(pool)
        .await?;
    Ok(())
}

/// Record that the gate of event `id` was checked now, with the reason no
/// item was created (`None` when one was).
pub async fn set_gate_checked(pool: &SqlitePool, id: i64, note: Option<&str>) -> Result<()> {
    sqlx::query("UPDATE events SET gate_checked_at = ?2, gate_note = ?3 WHERE id = ?1")
        .bind(id)
        .bind(crate::timestamp::now())
        .bind(note)
        .execute(pool)
        .await?;
    Ok(())
}

/// Newest first.
pub async fn list_events(pool: &SqlitePool, limit: i64) -> Result<Vec<Event>> {
    let rows = sqlx::query(&format!("SELECT {EVENT_COLUMNS} FROM events ORDER BY last_seen_at DESC LIMIT ?1"))
        .bind(limit)
        .fetch_all(pool)
        .await?;
    rows.iter().map(event_from_row).collect()
}

// ── items ────────────────────────────────────────────────────────────────

/// One pipeline item (`#id`). `stage` is the text form of [`Stage`]; the
/// dashboard narrows it to a union.
#[derive(Debug, Clone, Serialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export, rename = "IntakeItem")]
pub struct Item {
    #[ts(type = "number")]
    pub id: i64,
    #[ts(type = "number")]
    pub event_id: i64,
    /// `owner/name` of the configured repo the item works on.
    pub repo: String,
    pub title: String,
    pub stage: String,
    /// The stage a failed item failed in (what `retry` resumes).
    pub failed_stage: Option<String>,
    pub error: Option<String>,
    /// The effective class after the eval: `simple`, `complex`, `feature`.
    pub classification: Option<String>,
    /// [`super::stage::EvalResult`] as JSON.
    pub eval_json: Option<String>,
    /// The latest plan the refinement agent proposed.
    pub plan_draft: Option<String>,
    #[ts(type = "number")]
    pub plan_version: i64,
    /// The plan the operator approved: the implementation brief.
    pub approved_plan: Option<String>,
    #[ts(type = "number | null")]
    pub approved_version: Option<i64>,
    pub approved_at: Option<String>,
    pub approved_via: Option<String>,
    pub branch: Option<String>,
    /// The item's git worktree (under the configured work dir).
    pub worktree: Option<String>,
    /// The branch pull requests target.
    pub base_ref: Option<String>,
    /// The implementation agent's final message.
    pub impl_summary: Option<String>,
    /// The commit Nucleus collected from the item's clone (the agent's
    /// commits plus its uncommitted changes); exactly this commit is pushed.
    pub head_sha: Option<String>,
    /// `passed`, `failed`, `timeout` or `not_run` (Nucleus's own run).
    pub tests_status: Option<String>,
    pub tests_output: Option<String>,
    pub pr_url: Option<String>,
    pub comment_draft: Option<String>,
    /// `none`, `proposed`, `approved`, `posted`, `skipped`.
    pub comment_state: String,
    pub comment_url: Option<String>,
    /// Where the item's thread runs on WhatsApp: `none` (not yet),
    /// `pending` (group requested), `group`, `dm`.
    pub surface: String,
    pub group_requested_at: Option<String>,
    pub group_jid: Option<String>,
    pub group_closed_at: Option<String>,
    /// The stage task running now.
    pub current_task_id: Option<String>,
    /// The task the next stage task names as its parent.
    pub last_task_id: Option<String>,
    /// Consecutive failed attempts of the current step (a fetch, a push);
    /// the item fails at 3.
    #[ts(type = "number")]
    pub step_errors: i64,
    pub created_at: String,
    pub updated_at: String,
    pub closed_at: Option<String>,
    /// The event's title and body when the gate was satisfied: the only
    /// issue text any brief uses.
    pub rev_title: Option<String>,
    pub rev_body: Option<String>,
    /// [`super::event::revision_hash`] of `rev_title` and `rev_body`. The
    /// item goes `stale` when the source's text no longer matches it.
    pub revision_hash: Option<String>,
    /// The source event that opened the gate (GitHub: `labeled:<id>` or
    /// `reopened:<id>`; `accept:<time>` for `nucleus events emit --accept`).
    pub gate_event_id: Option<String>,
    /// The label event the gate depends on (`labeled:<id>`).
    pub label_event_id: Option<String>,
    /// Who set the gate (GitHub: the collaborator who added the label).
    pub gate_actor: Option<String>,
    pub gate_at: Option<String>,
    /// Why the item stopped as `stale`.
    pub stale_reason: Option<String>,
}

impl Item {
    pub fn stage(&self) -> Stage {
        Stage::parse(&self.stage).unwrap_or(Stage::Failed)
    }

    /// `#12`.
    pub fn tag(&self) -> String {
        format!("#{}", self.id)
    }
}

/// A message of an item's thread.
#[derive(Debug, Clone, Serialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export, rename = "IntakeMessage")]
pub struct ItemMessage {
    #[ts(type = "number")]
    pub id: i64,
    #[ts(type = "number")]
    pub item_id: i64,
    pub at: String,
    /// `operator`, `agent` or `nucleus`.
    pub author: String,
    /// `whatsapp`, `dashboard`, `cli` or `pipeline`.
    pub via: String,
    pub body: String,
    /// An operator message no refinement turn has read yet.
    #[ts(type = "number")]
    pub pending_agent: i64,
    pub read_by_task: Option<String>,
    /// `null` (to be sent to WhatsApp), `queued`, or `none` (not sent: it
    /// came from WhatsApp).
    pub wa_state: Option<String>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export, rename = "IntakeTransition")]
pub struct ItemTransition {
    #[ts(type = "number")]
    pub id: i64,
    #[ts(type = "number")]
    pub item_id: i64,
    pub at: String,
    pub from_stage: Option<String>,
    pub to_stage: String,
    pub reason: String,
}

/// A task of an item (every stage task, oldest first).
#[derive(Debug, Clone, Serialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export, rename = "IntakeItemTask")]
pub struct ItemTask {
    #[ts(type = "number")]
    pub item_id: i64,
    pub task_id: String,
    pub stage: String,
    pub created_at: String,
}

const ITEM_COLUMNS: &str = "id, event_id, repo, title, stage, failed_stage, error, classification, eval_json, \
    plan_draft, plan_version, approved_plan, approved_version, approved_at, approved_via, branch, worktree, \
    base_ref, impl_summary, head_sha, tests_status, tests_output, pr_url, comment_draft, comment_state, comment_url, \
    surface, group_requested_at, group_jid, group_closed_at, current_task_id, last_task_id, step_errors, \
    created_at, updated_at, closed_at, rev_title, rev_body, revision_hash, gate_event_id, label_event_id, \
    gate_actor, gate_at, stale_reason";

/// What a new item is bound to: the event's revision and the gate event.
#[derive(Debug, Clone)]
pub struct NewItem<'a> {
    pub event: &'a Event,
    pub repo: &'a str,
    pub rev_title: &'a str,
    pub rev_body: &'a str,
    pub gate_event_id: &'a str,
    pub label_event_id: Option<&'a str>,
    pub gate_actor: &'a str,
    pub gate_at: &'a str,
}

/// Create an item for an accepted event, in the `queued` stage, bound to
/// the given revision and gate event. Returns `None` when the event
/// already has an open item or an item for this gate event.
pub async fn create_item(pool: &SqlitePool, n: &NewItem<'_>) -> Result<Option<Item>> {
    let now = crate::timestamp::now();
    let hash = super::event::revision_hash(n.rev_title, n.rev_body);
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let res = sqlx::query(
        "INSERT OR IGNORE INTO items (event_id, repo, title, stage, created_at, updated_at, rev_title, rev_body,
                                      revision_hash, gate_event_id, label_event_id, gate_actor, gate_at)
         VALUES (?1, ?2, ?3, 'queued', ?4, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )
    .bind(n.event.id)
    .bind(n.repo)
    .bind(super::clip(n.rev_title, 200))
    .bind(&now)
    .bind(n.rev_title)
    .bind(n.rev_body)
    .bind(&hash)
    .bind(n.gate_event_id)
    .bind(n.label_event_id)
    .bind(n.gate_actor)
    .bind(n.gate_at)
    .execute(&mut *tx)
    .await?;
    if res.rows_affected() == 0 {
        return Ok(None);
    }
    let id = res.last_insert_rowid();
    sqlx::query(
        "INSERT INTO item_transitions (item_id, at, from_stage, to_stage, reason) VALUES (?1, ?2, NULL, 'queued', ?3)",
    )
    .bind(id)
    .bind(&now)
    .bind(format!(
        "accepted {} event {} (gate {} by {})",
        n.event.source, n.event.external_id, n.gate_event_id, n.gate_actor
    ))
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE events SET gate_checked_at = ?2, gate_note = NULL WHERE id = ?1")
        .bind(n.event.id)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(item(pool, id).await?))
}

pub async fn item(pool: &SqlitePool, id: i64) -> Result<Item> {
    sqlx::query_as(&format!("SELECT {ITEM_COLUMNS} FROM items WHERE id = ?1"))
        .bind(id)
        .fetch_optional(pool)
        .await?
        .with_context(|| format!("no item #{id}"))
}

/// The event's open item (not closed, cancelled or stale), if any.
pub async fn open_item_for_event(pool: &SqlitePool, event_id: i64) -> Result<Option<Item>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {ITEM_COLUMNS} FROM items WHERE event_id = ?1 AND stage NOT IN ('closed','cancelled','stale')"
    ))
    .bind(event_id)
    .fetch_optional(pool)
    .await?)
}

/// Every item of an event, oldest first.
pub async fn items_for_event(pool: &SqlitePool, event_id: i64) -> Result<Vec<Item>> {
    Ok(sqlx::query_as(&format!("SELECT {ITEM_COLUMNS} FROM items WHERE event_id = ?1 ORDER BY id"))
        .bind(event_id)
        .fetch_all(pool)
        .await?)
}

/// Ids of the items a tick advances (not closed, cancelled or stale).
pub async fn active_item_ids(pool: &SqlitePool) -> Result<Vec<i64>> {
    Ok(sqlx::query_scalar("SELECT id FROM items WHERE stage NOT IN ('closed','cancelled','stale') ORDER BY id")
        .fetch_all(pool)
        .await?)
}

/// Newest first. `open_only` leaves out closed and cancelled items, and
/// stale items that a newer item of the same event replaced (a stale item
/// waits for the operator until then).
pub async fn list_items(pool: &SqlitePool, open_only: bool, limit: i64) -> Result<Vec<Item>> {
    let filter = if open_only {
        "WHERE stage NOT IN ('closed','cancelled') AND NOT (stage = 'stale' AND EXISTS \
         (SELECT 1 FROM items b WHERE b.event_id = items.event_id AND b.id > items.id))"
    } else {
        ""
    };
    Ok(sqlx::query_as(&format!("SELECT {ITEM_COLUMNS} FROM items {filter} ORDER BY id DESC LIMIT ?1"))
        .bind(limit)
        .fetch_all(pool)
        .await?)
}

/// A column value for [`advance`] / [`update`].
#[derive(Debug, Clone)]
pub enum Val {
    Text(Option<String>),
    Int(Option<i64>),
}

impl From<&str> for Val {
    fn from(s: &str) -> Self {
        Val::Text(Some(s.to_string()))
    }
}
impl From<String> for Val {
    fn from(s: String) -> Self {
        Val::Text(Some(s))
    }
}
impl From<Option<String>> for Val {
    fn from(s: Option<String>) -> Self {
        Val::Text(s)
    }
}
impl From<i64> for Val {
    fn from(n: i64) -> Self {
        Val::Int(Some(n))
    }
}

/// Columns [`advance`] and [`update`] may set. `stage`, `id`, `event_id`,
/// `created_at` are not among them.
const SETTABLE: &[&str] = &[
    "failed_stage",
    "error",
    "classification",
    "eval_json",
    "plan_draft",
    "plan_version",
    "approved_plan",
    "approved_version",
    "approved_at",
    "approved_via",
    "branch",
    "worktree",
    "base_ref",
    "impl_summary",
    "head_sha",
    "tests_status",
    "tests_output",
    "pr_url",
    "comment_draft",
    "comment_state",
    "comment_url",
    "surface",
    "group_requested_at",
    "group_jid",
    "group_closed_at",
    "current_task_id",
    "last_task_id",
    "step_errors",
    "title",
    "closed_at",
    "stale_reason",
];

fn set_clause(set: &[(&str, Val)], first_param: usize) -> Result<String> {
    let mut parts = Vec::new();
    for (i, (col, _)) in set.iter().enumerate() {
        if !SETTABLE.contains(col) {
            bail!("item column {col:?} cannot be set");
        }
        parts.push(format!("{col} = ?{}", first_param + i));
    }
    Ok(parts.join(", "))
}

fn bind_vals<'q>(
    mut q: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>,
    set: &'q [(&str, Val)],
) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>> {
    for (_, v) in set {
        q = match v {
            Val::Text(t) => q.bind(t.clone()),
            Val::Int(n) => q.bind(*n),
        };
    }
    q
}

/// Move item `id` from stage `from` by `ev` (see [`transition`]), setting
/// `set` in the same statement and logging the transition, in one
/// `BEGIN IMMEDIATE` transaction. Returns `false` when the item is no
/// longer in `from` (another process moved it first).
///
/// Moving to `failed` records `from` as the failed stage; a retry clears
/// the failure; a terminal stage records `closed_at`.
pub async fn advance(
    pool: &SqlitePool,
    id: i64,
    from: Stage,
    ev: StageEvent,
    reason: &str,
    mut set: Vec<(&str, Val)>,
) -> Result<bool> {
    let to = transition(from, &ev)?;
    let now = crate::timestamp::now();
    if to == Stage::Failed {
        set.push(("failed_stage", from.as_str().into()));
        if !set.iter().any(|(c, _)| *c == "error") {
            set.push(("error", reason.into()));
        }
    }
    if matches!(ev, StageEvent::Retry { .. }) {
        set.push(("failed_stage", Val::Text(None)));
        set.push(("error", Val::Text(None)));
    }
    if to.is_terminal() {
        set.push(("closed_at", now.clone().into()));
    }
    let extra = set_clause(&set, 5)?;
    let sql = format!(
        "UPDATE items SET stage = ?2, updated_at = ?3{}{extra} WHERE id = ?1 AND stage = ?4",
        if extra.is_empty() { "" } else { ", " }
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let q = sqlx::query(&sql).bind(id).bind(to.as_str()).bind(&now).bind(from.as_str());
    let moved = bind_vals(q, &set).execute(&mut *tx).await?.rows_affected() == 1;
    if moved {
        sqlx::query(
            "INSERT INTO item_transitions (item_id, at, from_stage, to_stage, reason) VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(id)
        .bind(&now)
        .bind(from.as_str())
        .bind(to.as_str())
        .bind(reason)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(moved)
}

/// Set columns of item `id` while it is in stage `stage`, without a stage
/// change. Returns `false` when the item left that stage.
pub async fn update(pool: &SqlitePool, id: i64, stage: Stage, set: Vec<(&str, Val)>) -> Result<bool> {
    let extra = set_clause(&set, 4)?;
    if extra.is_empty() {
        return Ok(true);
    }
    let sql = format!("UPDATE items SET updated_at = ?2, {extra} WHERE id = ?1 AND stage = ?3");
    let q = sqlx::query(&sql).bind(id).bind(crate::timestamp::now()).bind(stage.as_str());
    Ok(bind_vals(q, &set).execute(pool).await?.rows_affected() == 1)
}

/// Record that `task_id` is an item's task for `stage`.
pub async fn add_item_task(pool: &SqlitePool, item_id: i64, task_id: &str, stage: Stage) -> Result<()> {
    sqlx::query("INSERT OR IGNORE INTO item_tasks (item_id, task_id, stage, created_at) VALUES (?1, ?2, ?3, ?4)")
        .bind(item_id)
        .bind(task_id)
        .bind(stage.as_str())
        .bind(crate::timestamp::now())
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn item_tasks(pool: &SqlitePool, item_id: i64) -> Result<Vec<ItemTask>> {
    Ok(sqlx::query_as("SELECT * FROM item_tasks WHERE item_id = ?1 ORDER BY created_at, task_id")
        .bind(item_id)
        .fetch_all(pool)
        .await?)
}

pub async fn transitions(pool: &SqlitePool, item_id: i64) -> Result<Vec<ItemTransition>> {
    Ok(sqlx::query_as("SELECT * FROM item_transitions WHERE item_id = ?1 ORDER BY id")
        .bind(item_id)
        .fetch_all(pool)
        .await?)
}

// ── thread ───────────────────────────────────────────────────────────────

/// A new thread message.
pub struct NewMessage<'a> {
    pub author: &'a str,
    pub via: &'a str,
    pub body: &'a str,
    /// Unique reference from the message's origin (a WhatsApp message id):
    /// a second insert with the same reference is ignored.
    pub external_ref: Option<&'a str>,
    /// An operator message that the next refinement turn must read.
    pub pending_agent: bool,
    /// Send it to the item's WhatsApp thread (`false`: it came from there).
    pub to_whatsapp: bool,
}

/// Append a message to item `id`'s thread. Returns its id, or `None` when
/// `external_ref` was seen before.
pub async fn add_message(pool: &SqlitePool, id: i64, m: NewMessage<'_>) -> Result<Option<i64>> {
    let res = sqlx::query(
        "INSERT OR IGNORE INTO item_messages (item_id, at, author, via, body, external_ref, pending_agent, wa_state)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(id)
    .bind(crate::timestamp::now())
    .bind(m.author)
    .bind(m.via)
    .bind(m.body)
    .bind(m.external_ref)
    .bind(m.pending_agent as i64)
    .bind(if m.to_whatsapp { None } else { Some("none") })
    .execute(pool)
    .await?;
    Ok((res.rows_affected() == 1).then(|| res.last_insert_rowid()))
}

pub async fn messages(pool: &SqlitePool, id: i64) -> Result<Vec<ItemMessage>> {
    Ok(sqlx::query_as(
        "SELECT id, item_id, at, author, via, body, pending_agent, read_by_task, wa_state
           FROM item_messages WHERE item_id = ?1 ORDER BY id",
    )
    .bind(id)
    .fetch_all(pool)
    .await?)
}

/// Mark the pending operator messages up to `up_to` as read by `task_id`.
pub async fn mark_read(pool: &SqlitePool, id: i64, up_to: i64, task_id: &str) -> Result<()> {
    sqlx::query(
        "UPDATE item_messages SET pending_agent = 0, read_by_task = ?3
          WHERE item_id = ?1 AND id <= ?2 AND pending_agent = 1",
    )
    .bind(id)
    .bind(up_to)
    .bind(task_id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Messages a failed refinement turn had read become pending again.
pub async fn unread(pool: &SqlitePool, id: i64, task_id: &str) -> Result<()> {
    sqlx::query("UPDATE item_messages SET pending_agent = 1, read_by_task = NULL WHERE item_id = ?1 AND read_by_task = ?2")
        .bind(id)
        .bind(task_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn set_wa_queued(pool: &SqlitePool, message_id: i64, outbound: i64) -> Result<()> {
    sqlx::query("UPDATE item_messages SET wa_state = 'queued', wa_outbound = ?2 WHERE id = ?1")
        .bind(message_id)
        .bind(outbound)
        .execute(pool)
        .await?;
    Ok(())
}

// ── comment binding ──────────────────────────────────────────────────────

/// What [`bind_comment`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentBinding {
    /// First use: recorded.
    New,
    /// Used before with the same content.
    Same,
    /// Used before with different content: the item must not continue.
    Changed,
}

/// Bind item `id` to a trusted comment's content (by comment id). The
/// first use records its hash; a later use compares.
pub async fn bind_comment(pool: &SqlitePool, id: i64, comment_id: &str, author: &str, body_hash: &str) -> Result<CommentBinding> {
    let res = sqlx::query(
        "INSERT OR IGNORE INTO item_comments (item_id, comment_id, author, body_hash, recorded_at) VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .bind(id)
    .bind(comment_id)
    .bind(author)
    .bind(body_hash)
    .bind(crate::timestamp::now())
    .execute(pool)
    .await?;
    if res.rows_affected() == 1 {
        return Ok(CommentBinding::New);
    }
    let stored: String = sqlx::query_scalar("SELECT body_hash FROM item_comments WHERE item_id = ?1 AND comment_id = ?2")
        .bind(id)
        .bind(comment_id)
        .fetch_one(pool)
        .await?;
    Ok(if stored == body_hash { CommentBinding::Same } else { CommentBinding::Changed })
}

/// The ids of every comment item `id` used.
pub async fn bound_comments(pool: &SqlitePool, id: i64) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT comment_id FROM item_comments WHERE item_id = ?1 ORDER BY comment_id")
        .bind(id)
        .fetch_all(pool)
        .await?)
}

// ── collaborators, meta ──────────────────────────────────────────────────

/// A cached collaborator decision younger than `max_age_secs`.
pub async fn cached_collaborator(pool: &SqlitePool, repo: &str, login: &str, max_age_secs: u64) -> Result<Option<bool>> {
    let row: Option<(i64, String)> =
        sqlx::query_as("SELECT trusted, checked_at FROM collaborators WHERE repo = ?1 AND login = ?2")
            .bind(repo.to_lowercase())
            .bind(login.to_lowercase())
            .fetch_optional(pool)
            .await?;
    Ok(row.and_then(|(trusted, at)| {
        let at = chrono::DateTime::parse_from_rfc3339(&at).ok()?;
        let age = chrono::Utc::now() - at.with_timezone(&chrono::Utc);
        (age.num_seconds() >= 0 && (age.num_seconds() as u64) < max_age_secs).then_some(trusted != 0)
    }))
}

pub async fn cache_collaborator(pool: &SqlitePool, repo: &str, login: &str, trusted: bool) -> Result<()> {
    sqlx::query(
        "INSERT INTO collaborators (repo, login, trusted, checked_at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(repo, login) DO UPDATE SET trusted = excluded.trusted, checked_at = excluded.checked_at",
    )
    .bind(repo.to_lowercase())
    .bind(login.to_lowercase())
    .bind(trusted as i64)
    .bind(crate::timestamp::now())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn meta(pool: &SqlitePool, key: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT value FROM meta WHERE key = ?1").bind(key).fetch_optional(pool).await?)
}

pub async fn set_meta(pool: &SqlitePool, key: &str, value: &str) -> Result<()> {
    sqlx::query("INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(key)
        .bind(value)
        .execute(pool)
        .await?;
    Ok(())
}

/// When each WhatsApp group was requested (the group budget).
pub async fn group_request_times(pool: &SqlitePool) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar("SELECT group_requested_at FROM items WHERE group_requested_at IS NOT NULL")
        .fetch_all(pool)
        .await?)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn issue(n: u32, labels: &[&str], state: &str) -> NewEvent {
        NewEvent {
            source: "github".into(),
            external_id: format!("acme/widget#{n}"),
            project: Some("acme/widget".into()),
            kind: "issue".into(),
            title: format!("Issue {n}"),
            body: "body".into(),
            author: Some("someone".into()),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            url: Some(format!("https://example.invalid/acme/widget/issues/{n}")),
            state: state.into(),
            created_at: Some("2026-09-20T10:00:00Z".into()),
            updated_at: Some("2026-09-20T10:00:00Z".into()),
            raw: serde_json::json!({ "number": n }),
            accepted: labels.contains(&"nucleus"),
        }
    }

    /// An item for `ev` bound to its current text, as the gate would.
    pub(crate) async fn new_item(pool: &SqlitePool, ev: &Event, gate: &str) -> Option<Item> {
        create_item(
            pool,
            &NewItem {
                event: ev,
                repo: "acme/widget",
                rev_title: &ev.title,
                rev_body: &ev.body,
                gate_event_id: gate,
                label_event_id: Some(gate),
                gate_actor: "maintainer",
                gate_at: "2026-09-20T10:05:00.000Z",
            },
        )
        .await
        .unwrap()
    }

    pub(crate) async fn temp_db() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        let pool = open(dir.path()).await.unwrap();
        (dir, pool)
    }

    #[tokio::test]
    async fn events_are_deduplicated_by_source_and_external_id() {
        let (_d, pool) = temp_db().await;
        let (a, k, _) = upsert_event(&pool, &issue(1, &[], "open")).await.unwrap();
        assert_eq!(k, Upsert::Inserted);
        let (b, k, _) = upsert_event(&pool, &issue(1, &[], "open")).await.unwrap();
        assert_eq!((k, b.id), (Upsert::Unchanged, a.id));
        let (c, k, _) = upsert_event(&pool, &issue(1, &["nucleus"], "open")).await.unwrap();
        assert_eq!((k, c.id), (Upsert::Updated, a.id));
        assert!(c.accepted && c.labels == vec!["nucleus".to_string()]);
        // Same external id from another source is another event.
        let mut other = issue(1, &[], "open");
        other.source = "cli".into();
        let (d, k, _) = upsert_event(&pool, &other).await.unwrap();
        assert_eq!(k, Upsert::Inserted);
        assert_ne!(d.id, a.id);
        assert_eq!(list_events(&pool, 10).await.unwrap().len(), 2);
        // Timestamps are stored in the sortable form.
        assert_eq!(c.created_at.as_deref(), Some("2026-09-20T10:00:00.000Z"));
    }

    #[tokio::test]
    async fn invalid_events_are_refused() {
        let (_d, pool) = temp_db().await;
        let mut e = issue(1, &[], "open");
        e.source = "bad source".into();
        assert!(upsert_event(&pool, &e).await.is_err());
        let mut e = issue(1, &[], "open");
        e.state = "merged".into();
        assert!(upsert_event(&pool, &e).await.is_err());
        let mut e = issue(1, &[], "open");
        e.title = " ".into();
        assert!(upsert_event(&pool, &e).await.is_err());
    }

    #[tokio::test]
    async fn one_item_per_event_and_guarded_stage_changes() {
        let (_d, pool) = temp_db().await;
        let (ev, _, _) = upsert_event(&pool, &issue(2, &["nucleus"], "open")).await.unwrap();
        let it = new_item(&pool, &ev, "labeled:1").await.unwrap();
        assert_eq!((it.stage(), it.id), (Stage::Queued, 1));
        assert_eq!(it.revision_hash.as_deref(), Some(super::super::event::revision_hash("Issue 2", "body").as_str()));
        assert!(new_item(&pool, &ev, "labeled:1").await.is_none(), "one item per gate event");
        assert!(new_item(&pool, &ev, "labeled:2").await.is_none(), "one open item per event");
        assert!(advance(&pool, it.id, Stage::Queued, StageEvent::EvalStarted, "eval", vec![]).await.unwrap());
        // A second process that read `queued` loses.
        assert!(!advance(&pool, it.id, Stage::Queued, StageEvent::EvalStarted, "eval", vec![]).await.unwrap());
        // Failing records the stage and the error; retry clears them.
        assert!(advance(&pool, it.id, Stage::Eval, StageEvent::Failed, "boom", vec![]).await.unwrap());
        let it = item(&pool, it.id).await.unwrap();
        assert_eq!((it.stage(), it.failed_stage.as_deref(), it.error.as_deref()), (Stage::Failed, Some("eval"), Some("boom")));
        assert!(advance(&pool, it.id, Stage::Failed, StageEvent::Retry { failed_in: Stage::Eval }, "retry", vec![])
            .await
            .unwrap());
        let it = item(&pool, it.id).await.unwrap();
        assert_eq!((it.stage(), it.failed_stage, it.error), (Stage::Queued, None, None));
        // Unknown columns are refused; allowed ones set.
        assert!(update(&pool, it.id, Stage::Queued, vec![("stage", "closed".into())]).await.is_err());
        assert!(update(&pool, it.id, Stage::Queued, vec![("branch", "b".into())]).await.unwrap());
        assert!(!update(&pool, it.id, Stage::Eval, vec![("branch", "c".into())]).await.unwrap());
        assert!(advance(&pool, it.id, Stage::Queued, StageEvent::Cancel, "stop", vec![]).await.unwrap());
        let it = item(&pool, it.id).await.unwrap();
        assert!(it.closed_at.is_some() && it.stage() == Stage::Cancelled);
        // A closed item lets a new gate event start a new item; the same
        // gate event never does.
        assert!(new_item(&pool, &ev, "labeled:1").await.is_none());
        let second = new_item(&pool, &ev, "reopened:9").await.unwrap();
        assert_eq!(second.id, 2);
        assert_eq!(open_item_for_event(&pool, ev.id).await.unwrap().unwrap().id, 2);
        assert_eq!(items_for_event(&pool, ev.id).await.unwrap().len(), 2);
        let log = transitions(&pool, it.id).await.unwrap();
        let path: Vec<&str> = log.iter().map(|t| t.to_stage.as_str()).collect();
        assert_eq!(path, ["queued", "eval", "failed", "queued", "cancelled"]);
    }

    #[tokio::test]
    async fn thread_messages_dedup_and_read_marks() {
        let (_d, pool) = temp_db().await;
        let (ev, _, _) = upsert_event(&pool, &issue(3, &["nucleus"], "open")).await.unwrap();
        let it = new_item(&pool, &ev, "labeled:1").await.unwrap();
        let m = |r: Option<&'static str>| NewMessage {
            author: "operator",
            via: "whatsapp",
            body: "hi",
            external_ref: r,
            pending_agent: true,
            to_whatsapp: false,
        };
        let a = add_message(&pool, it.id, m(Some("wa:1"))).await.unwrap();
        assert!(a.is_some());
        assert!(add_message(&pool, it.id, m(Some("wa:1"))).await.unwrap().is_none(), "same WhatsApp message");
        let b = add_message(&pool, it.id, m(None)).await.unwrap().unwrap();
        mark_read(&pool, it.id, a.unwrap(), "t1").await.unwrap();
        let all = messages(&pool, it.id).await.unwrap();
        assert_eq!(all.iter().map(|m| m.pending_agent).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(all[0].wa_state.as_deref(), Some("none"));
        unread(&pool, it.id, "t1").await.unwrap();
        assert!(messages(&pool, it.id).await.unwrap().iter().all(|m| m.pending_agent == 1));
        set_wa_queued(&pool, b, 7).await.unwrap();
    }

    #[tokio::test]
    async fn comments_are_bound_by_content() {
        let (_d, pool) = temp_db().await;
        let (ev, _, _) = upsert_event(&pool, &issue(4, &["nucleus"], "open")).await.unwrap();
        let it = new_item(&pool, &ev, "labeled:1").await.unwrap();
        assert_eq!(bind_comment(&pool, it.id, "55", "dev", "h1").await.unwrap(), CommentBinding::New);
        assert_eq!(bind_comment(&pool, it.id, "55", "dev", "h1").await.unwrap(), CommentBinding::Same);
        assert_eq!(bind_comment(&pool, it.id, "55", "dev", "h2").await.unwrap(), CommentBinding::Changed);
        assert_eq!(bound_comments(&pool, it.id).await.unwrap(), ["55"]);
    }

    #[tokio::test]
    async fn v3_rebuilds_items_and_keeps_every_row() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(super::super::INTAKE_DB_PATH);
        let pool = crate::db::open(&path).await.unwrap();
        crate::migrate::migrate(
            &pool,
            &[
                crate::migrate::Migration { version: 1, name: "intake baseline", step: crate::migrate::Step::Sql(SCHEMA_V1) },
                crate::migrate::Migration { version: 2, name: "collected commit", step: crate::migrate::Step::Sql(SCHEMA_V2) },
            ],
        )
        .await
        .unwrap();
        for sql in [
            "INSERT INTO events (source, external_id, kind, title, body, labels_json, state, raw_json, accepted, first_seen_at, last_seen_at)
             VALUES ('github', 'acme/widget#1', 'issue', 'T', 'B', '[]', 'open', '{}', 1, 't', 't')",
            "INSERT INTO items (event_id, repo, title, stage, created_at, updated_at, head_sha) VALUES (1, 'acme/widget', 'T', 'eval', 't', 't', 'abc')",
            "INSERT INTO item_messages (item_id, at, author, via, body) VALUES (1, 't', 'nucleus', 'pipeline', 'hi')",
        ] {
            sqlx::query(sql).execute(&pool).await.unwrap();
        }
        pool.close().await;
        let pool = open(dir.path()).await.unwrap();
        let it = item(&pool, 1).await.unwrap();
        assert_eq!((it.stage.as_str(), it.head_sha.as_deref(), it.revision_hash), ("eval", Some("abc"), None));
        assert_eq!(messages(&pool, 1).await.unwrap().len(), 1);
        let fk: i64 = sqlx::query_scalar("PRAGMA foreign_keys").fetch_one(&pool).await.unwrap();
        assert_eq!(fk, 1, "foreign keys are on again");
        // Running open again is a no-op.
        drop(open(dir.path()).await.unwrap());
    }

    #[tokio::test]
    async fn collaborator_cache_expires() {
        let (_d, pool) = temp_db().await;
        assert_eq!(cached_collaborator(&pool, "acme/widget", "Dev", 60).await.unwrap(), None);
        cache_collaborator(&pool, "Acme/Widget", "dev", true).await.unwrap();
        assert_eq!(cached_collaborator(&pool, "acme/widget", "DEV", 60).await.unwrap(), Some(true));
        assert_eq!(cached_collaborator(&pool, "acme/widget", "dev", 0).await.unwrap(), None);
    }
}
