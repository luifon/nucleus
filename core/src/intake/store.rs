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

/// The whole intake.db schema. One version: the feature was never deployed
/// before its review rounds, so the intermediate versions were collapsed.
///
/// - `events`: one row per `(source, external_id)`; `changed_at` is when the
///   content last changed, `gate_checked_at` / `gate_note` record the last
///   gate check and why it created no item.
/// - `items`: an event can have several items over time (the label added
///   again, the issue reopened), at most one open (not closed, cancelled or
///   stale) and one per gate event. Each item is bound to the revision of
///   its event when the gate was satisfied (`rev_title`, `rev_body`,
///   `revision_hash`) and to the gate event itself.
/// - `item_comments`: the content hash of every trusted comment an item used.
/// - `inbound_commands`: the processing state of every operator message read
///   from WhatsApp; the read watermark moves only past final ones.
const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS events (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    source          TEXT NOT NULL,
    external_id     TEXT NOT NULL,
    project         TEXT,
    kind            TEXT NOT NULL,
    title           TEXT NOT NULL,
    body            TEXT NOT NULL,
    author          TEXT,
    labels_json     TEXT NOT NULL,
    url             TEXT,
    state           TEXT NOT NULL,
    created_at      TEXT,
    updated_at      TEXT,
    raw_json        TEXT NOT NULL,
    accepted        INTEGER NOT NULL,
    first_seen_at   TEXT NOT NULL,
    last_seen_at    TEXT NOT NULL,
    changed_at      TEXT,
    gate_checked_at TEXT,
    gate_note       TEXT,
    UNIQUE (source, external_id)
);
CREATE TABLE IF NOT EXISTS items (
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
    comment_op         TEXT,
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
    base_sha           TEXT,
    pushed_sha         TEXT,
    rev_title          TEXT,
    rev_body           TEXT,
    revision_hash      TEXT,
    gate_event_id      TEXT,
    label_event_id     TEXT,
    gate_actor         TEXT,
    gate_at            TEXT,
    stale_reason       TEXT
);
CREATE INDEX IF NOT EXISTS idx_items_stage ON items(stage, id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_items_open_event ON items(event_id) WHERE stage NOT IN ('closed', 'cancelled', 'stale');
CREATE UNIQUE INDEX IF NOT EXISTS idx_items_gate ON items(event_id, gate_event_id) WHERE gate_event_id IS NOT NULL;
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
CREATE TABLE IF NOT EXISTS item_comments (
    item_id     INTEGER NOT NULL REFERENCES items(id),
    comment_id  TEXT NOT NULL,
    author      TEXT NOT NULL,
    body_hash   TEXT NOT NULL,
    recorded_at TEXT NOT NULL,
    PRIMARY KEY (item_id, comment_id)
);
CREATE TABLE IF NOT EXISTS inbound_commands (
    msg_ref     TEXT PRIMARY KEY,
    wa_row_id   INTEGER NOT NULL,
    item_key    TEXT NOT NULL,
    state       TEXT NOT NULL,
    attempts    INTEGER NOT NULL DEFAULT 0,
    error       TEXT,
    received_at TEXT NOT NULL,
    updated_at  TEXT NOT NULL
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

/// The hidden-content hold (ADR-036, "The hidden-content hold"): what an
/// item was held for, and what the operator released.
///
/// - `hold_stage`: the stage the item was held in (a release returns there).
/// - `hold_json`: the findings ([`super::hidden::Finding`] list, JSON).
/// - `hold_hash`: [`super::hidden::fingerprint`] of what the findings were
///   computed on; a release is refused when the source no longer matches.
/// - `released_hash`: the fingerprint the operator released; a later check
///   with the same fingerprint lets the item continue.
const SCHEMA_V2: &str = "
ALTER TABLE items ADD COLUMN hold_stage TEXT;
ALTER TABLE items ADD COLUMN hold_json TEXT;
ALTER TABLE items ADD COLUMN hold_hash TEXT;
ALTER TABLE items ADD COLUMN held_at TEXT;
ALTER TABLE items ADD COLUMN released_hash TEXT;
ALTER TABLE items ADD COLUMN released_at TEXT;
ALTER TABLE items ADD COLUMN released_via TEXT";

/// The issue comment needs no approval any more (ADR-036, "Operator
/// decisions"): the `review` stage and the proposed comment are gone, and
/// the `pr` stage posts the pull request link itself. Rows that exist are
/// moved as follows:
///
/// - an item in `review` whose comment was not posted (`none`, `proposed`,
///   `approved`) goes back to `pr`, which finds its pull request and posts
///   the link. `comment_op` is kept, so a comment an earlier attempt posted
///   is found by its marker and not posted twice;
/// - an item in `review` whose comment was `posted` or `skipped` would
///   have closed at the next tick: it is closed now;
/// - a failed or blocked item that stopped in `review` resumes in `pr`;
/// - `proposed` and `approved` become `none`; `comment_draft` is dropped.
///
/// Every stage change is logged in `item_transitions`.
const SCHEMA_V3: &str = "
INSERT INTO item_transitions (item_id, at, from_stage, to_stage, reason)
    SELECT id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), 'review', 'pr',
           'migrated: the issue comment needs no approval, the pr stage posts the pull request link'
      FROM items WHERE stage = 'review' AND comment_state NOT IN ('posted', 'skipped');
INSERT INTO item_transitions (item_id, at, from_stage, to_stage, reason)
    SELECT id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), 'review', 'closed',
           'migrated: the review stage was removed, the comment was already posted or skipped'
      FROM items WHERE stage = 'review' AND comment_state IN ('posted', 'skipped');
UPDATE items SET stage = 'closed', closed_at = COALESCE(closed_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
                 updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
    WHERE stage = 'review' AND comment_state IN ('posted', 'skipped');
UPDATE items SET stage = 'pr', updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE stage = 'review';
UPDATE items SET failed_stage = 'pr' WHERE failed_stage = 'review';
UPDATE items SET comment_state = 'none' WHERE comment_state IN ('proposed', 'approved');
ALTER TABLE items DROP COLUMN comment_draft";

/// Confirmation questions the pipeline asked the operator on WhatsApp
/// (ADR-036, "Operator decisions"). A question is asked in one place
/// (`scope`: `dm`; `group:<n>` until groups were removed, see [`SCHEMA_V5`]) and only the next answer from there
/// settles it, until `expires_at`. The decision is stored with what the
/// operator saw when he was asked: the plan version or the hold
/// fingerprint it binds to.
///
/// `state`: `pending`, then `confirmed` (the answer applied it), `refused`
/// (the answer was yes but the decision was refused), `declined`,
/// `replaced` (another message came instead of an answer) or `expired`.
/// `answer_ref` is the operator message being applied as the answer: set
/// before the decision runs, and the decision's own transaction closes the
/// question, so a crash never applies one answer twice.
const SCHEMA_V4: &str = "
CREATE TABLE confirmations (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    scope        TEXT NOT NULL,
    item_id      INTEGER NOT NULL REFERENCES items(id),
    decision     TEXT NOT NULL,
    plan_version INTEGER,
    hold_hash    TEXT,
    question     TEXT NOT NULL,
    asked_by     TEXT NOT NULL,
    answer_ref   TEXT,
    state        TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    expires_at   TEXT NOT NULL,
    closed_at    TEXT
);
CREATE INDEX idx_confirmations_scope ON confirmations(scope, state, id)";

/// Per-item WhatsApp groups are removed (ADR-036, "No WhatsApp
/// groups"): every item's WhatsApp surface is the operator's DM.
///
/// - an item whose thread ran in a group (`group`) or waited for one
///   (`pending`) now uses the DM;
/// - a confirmation question still open in a group scope can no longer be
///   answered there: it is closed as `replaced`;
/// - the group columns are dropped. The bot's own group tables in
///   whatsapp.db are dropped by the bot after it has left every group they
///   record (messaging/whatsapp/src/intake.ts).
const SCHEMA_V5: &str = "
UPDATE items SET surface = 'dm' WHERE surface IN ('group', 'pending');
UPDATE confirmations SET state = 'replaced', closed_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
    WHERE scope LIKE 'group:%' AND state = 'pending';
ALTER TABLE items DROP COLUMN group_requested_at;
ALTER TABLE items DROP COLUMN group_jid;
ALTER TABLE items DROP COLUMN group_closed_at";

/// WhatsApp gets short notices, not thread messages (ADR-036, "WhatsApp gets short
/// notices"). `item_messages.notice` is the code-owned notice a thread
/// message sends to the operator's DM (NULL: nothing is sent). Thread
/// messages still waiting to be copied to WhatsApp in full are not sent any
/// more: the long bodies stay on the dashboard.
const SCHEMA_V6: &str = "
ALTER TABLE item_messages ADD COLUMN notice TEXT;
UPDATE item_messages SET wa_state = 'none' WHERE wa_state IS NULL";

/// Plans are passed whole or refused, and every accepted plan version is
/// kept (ADR-036, "Plans are never cut"):
///
/// - `plan_versions`: one row per accepted plan version of an item, its
///   text whole and when the agent proposed it;
/// - `item_messages.plan_version`: the plan version an agent reply carried
///   (the brief's history shows a reference in its place);
/// - `items.plan_refused_chars`, `items.plan_refusals`: the length of the
///   latest proposed plan when it was refused for its length (NULL when the
///   latest reply was accepted), and how many replies in a row were.
///
/// Backfill, where known: the plan each existing agent reply carried (its
/// `── plan vN ──` block), then the item's `plan_draft` (the latest plan)
/// and `approved_plan`, which are the authoritative text of their versions.
/// A Rust step: it parses the shown replies. It is one transaction and
/// every statement is repeatable, so a crash before the version row is
/// recorded runs it again safely.
fn schema_v7(pool: &SqlitePool) -> futures::future::BoxFuture<'_, Result<()>> {
    Box::pin(async move {
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS plan_versions (
                item_id     INTEGER NOT NULL REFERENCES items(id),
                version     INTEGER NOT NULL,
                text        TEXT NOT NULL,
                proposed_at TEXT NOT NULL,
                PRIMARY KEY (item_id, version)
            )",
        )
        .execute(&mut *tx)
        .await?;
        for (table, col, ddl) in [
            ("item_messages", "plan_version", "ALTER TABLE item_messages ADD COLUMN plan_version INTEGER"),
            ("items", "plan_refused_chars", "ALTER TABLE items ADD COLUMN plan_refused_chars INTEGER"),
            ("items", "plan_refusals", "ALTER TABLE items ADD COLUMN plan_refusals INTEGER NOT NULL DEFAULT 0"),
        ] {
            let have: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"))
                .bind(col)
                .fetch_one(&mut *tx)
                .await?;
            if have == 0 {
                sqlx::query(ddl).execute(&mut *tx).await?;
            }
        }
        // The plan each agent reply carried.
        let replies: Vec<(i64, i64, String, String)> =
            sqlx::query_as("SELECT id, item_id, at, body FROM item_messages WHERE author = 'agent' AND plan_version IS NULL")
                .fetch_all(&mut *tx)
                .await?;
        for (id, item_id, at, body) in replies {
            let Some(v) = carried_plan_version(&body) else { continue };
            sqlx::query("UPDATE item_messages SET plan_version = ?2 WHERE id = ?1").bind(id).bind(v).execute(&mut *tx).await?;
            if let Some(text) = super::stage::shown_plan(&body, &super::stage::plan_label(v)).filter(|t| !t.is_empty()) {
                sqlx::query("INSERT OR IGNORE INTO plan_versions (item_id, version, text, proposed_at) VALUES (?1, ?2, ?3, ?4)")
                    .bind(item_id)
                    .bind(v)
                    .bind(text)
                    .bind(&at)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        // The item's own record of its approved and its latest plan. When
        // no reply dates a version, the approval (the plan was proposed
        // before it) or the item's last change stands in.
        for (text_col, version_col, at_fallback) in
            [("approved_plan", "approved_version", "COALESCE(i.approved_at, i.updated_at)"), ("plan_draft", "plan_version", "i.updated_at")]
        {
            sqlx::query(&format!(
                "INSERT INTO plan_versions (item_id, version, text, proposed_at)
                 SELECT i.id, i.{version_col}, i.{text_col},
                        COALESCE((SELECT MAX(m.at) FROM item_messages m
                                   WHERE m.item_id = i.id AND m.author = 'agent' AND m.plan_version = i.{version_col}),
                                 {at_fallback})
                   FROM items i
                  WHERE i.{text_col} IS NOT NULL AND i.{version_col} IS NOT NULL AND i.{version_col} > 0
                 ON CONFLICT (item_id, version) DO UPDATE SET text = excluded.text"
            ))
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    })
}

/// The plan version a shown agent reply carries: the version of its last
/// `── plan vN ──` line that has a matching end line after it.
fn carried_plan_version(body: &str) -> Option<i64> {
    let mut found = None;
    for (i, _) in body.match_indices("── plan v") {
        let rest = &body[i + "── plan v".len()..];
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        let Ok(v) = digits.parse::<i64>() else { continue };
        if rest[digits.len()..].starts_with(" ──") && super::stage::shown_plan(body, &super::stage::plan_label(v)).is_some() {
            found = Some(v);
        }
    }
    found
}

/// Open (creating and migrating) intake.db. Writers only.
pub async fn open(workspace_root: &Path) -> Result<SqlitePool> {
    let pool = crate::db::open(&workspace_root.join(super::INTAKE_DB_PATH)).await?;
    crate::migrate::migrate(
        &pool,
        &[
            crate::migrate::Migration { version: 1, name: "intake schema", step: crate::migrate::Step::Sql(SCHEMA_V1) },
            crate::migrate::Migration { version: 2, name: "hidden-content hold", step: crate::migrate::Step::Sql(SCHEMA_V2) },
            crate::migrate::Migration { version: 3, name: "no comment approval", step: crate::migrate::Step::Sql(SCHEMA_V3) },
            crate::migrate::Migration { version: 4, name: "operator confirmations", step: crate::migrate::Step::Sql(SCHEMA_V4) },
            crate::migrate::Migration { version: 5, name: "no whatsapp groups", step: crate::migrate::Step::Sql(SCHEMA_V5) },
            crate::migrate::Migration { version: 6, name: "whatsapp notices", step: crate::migrate::Step::Sql(SCHEMA_V6) },
            crate::migrate::Migration { version: 7, name: "plan versions", step: crate::migrate::Step::Rust(schema_v7) },
        ],
    )
    .await
    .context("migrating intake.db")?;
    Ok(pool)
}

/// The schema version this code writes.
pub const SCHEMA_VERSION: i64 = 7;

/// True when `pool` (a read-only intake.db) has the full schema: the
/// migration ledger records [`SCHEMA_VERSION`] and the item tables exist.
/// An empty file, or a database a writer is still creating, is not ready.
pub async fn schema_ready(pool: &SqlitePool) -> bool {
    let tables: i64 = match sqlx::query_scalar(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('schema_migrations', 'events', 'items', 'item_messages', 'item_transitions', 'item_tasks')",
    )
    .fetch_one(pool)
    .await
    {
        Ok(n) => n,
        Err(_) => return false,
    };
    if tables != 6 {
        return false;
    }
    matches!(
        sqlx::query_scalar::<_, Option<i64>>("SELECT MAX(version) FROM schema_migrations").fetch_one(pool).await,
        Ok(Some(v)) if v >= SCHEMA_VERSION
    )
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
    /// The pull request link on the issue: `none` (not posted yet),
    /// `posted`, or `skipped` (the event's source has no reply channel).
    pub comment_state: String,
    pub comment_url: Option<String>,
    /// The random operation id of the issue comment (stored before it is
    /// posted; its marker line finds an earlier post after a crash).
    pub comment_op: Option<String>,
    /// Whether the item has a WhatsApp thread: `none` (nothing sent yet) or
    /// `dm` (its notices go to the operator's DM, marked `#<n>`).
    pub surface: String,
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
    /// The base commit the item's clone started from.
    pub base_sha: Option<String>,
    /// The commit Nucleus last pushed to the item's branch.
    pub pushed_sha: Option<String>,
    /// The stage a held item was held in (a release returns there).
    pub hold_stage: Option<String>,
    /// The hidden-content findings the item was last held for
    /// ([`super::hidden::Finding`] list, JSON).
    pub hold_json: Option<String>,
    /// [`super::hidden::fingerprint`] of what the findings were computed on.
    pub hold_hash: Option<String>,
    pub held_at: Option<String>,
    /// The fingerprint the operator released: the item continues while the
    /// hidden content it carries is exactly this.
    pub released_hash: Option<String>,
    pub released_at: Option<String>,
    pub released_via: Option<String>,
    /// The length of the latest proposed plan when it was refused for being
    /// longer than [`super::briefs::PLAN_LIMIT`]; `None` when the latest
    /// reply was accepted. The next refinement brief says so. Not part of
    /// the dashboard's wire type (the thread shows the refusal).
    #[serde(skip)]
    pub plan_refused_chars: Option<i64>,
    /// Refinement replies refused in a row for their plan's length.
    #[serde(skip)]
    pub plan_refusals: i64,
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
    /// `null` (its notice is still to be sent), `queued` (the notice is in
    /// the outbound queue), or `none` (nothing is sent for it).
    pub wa_state: Option<String>,
    /// The short, code-owned notice this message sends to the operator's
    /// WhatsApp DM (never the body). Not part of the dashboard's wire type.
    #[serde(skip)]
    pub notice: Option<String>,
    /// The plan version an agent reply carried (its plan is in the body
    /// between the `── plan vN ──` lines). Not part of the wire type: the
    /// dashboard reads the versions from the detail's `plans`.
    #[serde(skip)]
    pub plan_version: Option<i64>,
}

/// One accepted plan version of an item, whole.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export, rename = "IntakePlanVersion")]
pub struct PlanVersion {
    #[ts(type = "number")]
    pub version: i64,
    pub text: String,
    /// When the refinement agent proposed it.
    pub at: String,
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
    base_ref, impl_summary, head_sha, tests_status, tests_output, pr_url, comment_state, comment_url, comment_op, \
    surface, current_task_id, last_task_id, step_errors, \
    created_at, updated_at, closed_at, rev_title, rev_body, revision_hash, gate_event_id, label_event_id, \
    gate_actor, gate_at, stale_reason, base_sha, pushed_sha, hold_stage, hold_json, hold_hash, held_at, \
    released_hash, released_at, released_via, plan_refused_chars, plan_refusals";

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
    "comment_state",
    "comment_url",
    "comment_op",
    "surface",
    "current_task_id",
    "last_task_id",
    "step_errors",
    "title",
    "closed_at",
    "stale_reason",
    "base_sha",
    "pushed_sha",
    "hold_stage",
    "hold_json",
    "hold_hash",
    "held_at",
    "released_hash",
    "released_at",
    "released_via",
    "plan_refused_chars",
    "plan_refusals",
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
/// Moving to `failed` or `blocked` records `from` as the failed stage; a retry clears
/// the failure; a terminal stage records `closed_at`.
pub async fn advance(pool: &SqlitePool, id: i64, from: Stage, ev: StageEvent, reason: &str, set: Vec<(&str, Val)>) -> Result<bool> {
    advance_caused(pool, id, from, ev, reason, set, None).await
}

/// [`advance`] caused by operator message `cause` (an
/// [`inbound_commands`] key): the message is marked `applied` in the same
/// transaction, so a crash leaves both undone or both done.
pub async fn advance_caused(
    pool: &SqlitePool,
    id: i64,
    from: Stage,
    ev: StageEvent,
    reason: &str,
    set: Vec<(&str, Val)>,
    cause: Option<&str>,
) -> Result<bool> {
    advance_where(pool, id, from, ev, reason, set, cause, None).await
}

/// An extra condition a stage change checks in its own `UPDATE`.
enum Guard<'a> {
    /// The item's `hold_hash` is still this one.
    Hold(&'a str),
    /// The latest plan is still this version and no refinement turn runs.
    Plan(i64),
}

#[allow(clippy::too_many_arguments)]
async fn advance_where(
    pool: &SqlitePool,
    id: i64,
    from: Stage,
    ev: StageEvent,
    reason: &str,
    mut set: Vec<(&str, Val)>,
    cause: Option<&str>,
    guard: Option<Guard<'_>>,
) -> Result<bool> {
    let to = transition(from, &ev)?;
    let now = crate::timestamp::now();
    if to == Stage::Failed || to == Stage::Blocked {
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
    let guard_param = 5 + set.len();
    let sql = format!(
        "UPDATE items SET stage = ?2, updated_at = ?3{}{extra} WHERE id = ?1 AND stage = ?4{}",
        if extra.is_empty() { "" } else { ", " },
        match guard {
            Some(Guard::Hold(_)) => format!(" AND hold_hash = ?{guard_param}"),
            Some(Guard::Plan(_)) => format!(" AND plan_version = ?{guard_param} AND current_task_id IS NULL"),
            None => String::new(),
        }
    );
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let q = sqlx::query(&sql).bind(id).bind(to.as_str()).bind(&now).bind(from.as_str());
    let mut q = bind_vals(q, &set);
    match guard {
        Some(Guard::Hold(h)) => q = q.bind(h.to_string()),
        Some(Guard::Plan(v)) => q = q.bind(v),
        None => {}
    }
    let moved = q.execute(&mut *tx).await?.rows_affected() == 1;
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
        mark_applied(&mut tx, cause).await?;
    }
    tx.commit().await?;
    Ok(moved)
}

/// Release a held item (`ev` is a [`StageEvent::Release`]) only while its
/// `hold_hash` is still `hold`: the hold the operator reviewed. The check and
/// the stage change are one statement in one transaction, so a release
/// that races a new hold changes nothing and returns `false`.
#[allow(clippy::too_many_arguments)]
pub async fn advance_if_hold(
    pool: &SqlitePool,
    id: i64,
    ev: StageEvent,
    reason: &str,
    set: Vec<(&str, Val)>,
    cause: Option<&str>,
    hold: &str,
) -> Result<bool> {
    advance_where(pool, id, Stage::Held, ev, reason, set, cause, Some(Guard::Hold(hold))).await
}

/// Approve plan `version` of an item in refinement only while it is still
/// the latest plan and no refinement turn runs: the check and the stage
/// change are one statement, so a plan that arrives after the operator's
/// approval was read is never the one approved.
pub async fn advance_if_plan(
    pool: &SqlitePool,
    id: i64,
    reason: &str,
    set: Vec<(&str, Val)>,
    cause: Option<&str>,
    version: i64,
) -> Result<bool> {
    advance_where(pool, id, Stage::Refinement, StageEvent::PlanApproved, reason, set, cause, Some(Guard::Plan(version))).await
}

async fn mark_applied(tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>, cause: Option<&str>) -> Result<()> {
    if let Some(c) = cause {
        let now = crate::timestamp::now();
        sqlx::query("UPDATE inbound_commands SET state = 'applied', error = NULL, updated_at = ?2 WHERE msg_ref = ?1")
            .bind(c)
            .bind(&now)
            .execute(&mut **tx)
            .await?;
        // The message was the answer to a confirmation: applying it settles
        // the question in the same transaction.
        sqlx::query("UPDATE confirmations SET state = 'confirmed', closed_at = ?2 WHERE answer_ref = ?1 AND state = 'pending'")
            .bind(c)
            .bind(&now)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// Set columns of item `id` while it is in stage `stage`, without a stage
/// change. Returns `false` when the item left that stage.
pub async fn update(pool: &SqlitePool, id: i64, stage: Stage, set: Vec<(&str, Val)>) -> Result<bool> {
    update_caused(pool, id, stage, set, None).await
}

/// [`update`] caused by operator message `cause`, marked `applied` in the
/// same transaction when the update happens.
pub async fn update_caused(pool: &SqlitePool, id: i64, stage: Stage, set: Vec<(&str, Val)>, cause: Option<&str>) -> Result<bool> {
    let extra = set_clause(&set, 4)?;
    if extra.is_empty() {
        return Ok(true);
    }
    let sql = format!("UPDATE items SET updated_at = ?2, {extra} WHERE id = ?1 AND stage = ?3");
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let q = sqlx::query(&sql).bind(id).bind(crate::timestamp::now()).bind(stage.as_str());
    let done = bind_vals(q, &set).execute(&mut *tx).await?.rows_affected() == 1;
    if done {
        mark_applied(&mut tx, cause).await?;
    }
    tx.commit().await?;
    Ok(done)
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
    /// The short notice sent to the operator's WhatsApp DM for this message;
    /// `None` sends nothing. The body itself never goes to WhatsApp.
    pub notice: Option<String>,
}

/// Append a message to item `id`'s thread. Returns its id, or `None` when
/// `external_ref` was seen before.
pub async fn add_message(pool: &SqlitePool, id: i64, m: NewMessage<'_>) -> Result<Option<i64>> {
    add_message_caused(pool, id, m, None).await
}

/// [`add_message`] that also marks operator message `cause` applied, in
/// one transaction (a plain thread message is applied by being stored).
pub async fn add_message_caused(pool: &SqlitePool, id: i64, m: NewMessage<'_>, cause: Option<&str>) -> Result<Option<i64>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let added = insert_message(&mut tx, id, &m, None).await?;
    mark_applied(&mut tx, cause).await?;
    tx.commit().await?;
    Ok(added)
}

async fn insert_message(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    id: i64,
    m: &NewMessage<'_>,
    plan_version: Option<i64>,
) -> Result<Option<i64>> {
    let res = sqlx::query(
        "INSERT OR IGNORE INTO item_messages (item_id, at, author, via, body, external_ref, pending_agent, wa_state, notice, plan_version)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )
    .bind(id)
    .bind(crate::timestamp::now())
    .bind(m.author)
    .bind(m.via)
    .bind(m.body)
    .bind(m.external_ref)
    .bind(m.pending_agent as i64)
    .bind(if m.notice.is_some() { None } else { Some("none") })
    .bind(&m.notice)
    .bind(plan_version)
    .execute(&mut **tx)
    .await?;
    Ok((res.rows_affected() == 1).then(|| res.last_insert_rowid()))
}

/// What a finished refinement turn records.
pub struct TurnRecord<'a> {
    /// Thread messages, each with the plan version it carries.
    pub messages: Vec<(NewMessage<'a>, Option<i64>)>,
    /// A plan accepted as a new version: `(version, text)`.
    pub plan: Option<(i64, &'a str)>,
    /// Item columns to set.
    pub set: Vec<(&'a str, Val)>,
}

/// Record finished refinement turn `task_id` of item `id` in one
/// transaction: the item's columns, its thread messages and an accepted
/// plan version, only while the item is in refinement with that turn as its
/// current task. Returns `false` (and records nothing) when it is not.
pub async fn record_turn(pool: &SqlitePool, id: i64, task_id: &str, turn: TurnRecord<'_>) -> Result<bool> {
    let extra = set_clause(&turn.set, 4)?;
    let sql = format!(
        "UPDATE items SET updated_at = ?2{}{extra} WHERE id = ?1 AND stage = 'refinement' AND current_task_id = ?3",
        if extra.is_empty() { "" } else { ", " }
    );
    let now = crate::timestamp::now();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let q = sqlx::query(&sql).bind(id).bind(&now).bind(task_id);
    if bind_vals(q, &turn.set).execute(&mut *tx).await?.rows_affected() != 1 {
        tx.rollback().await?;
        return Ok(false);
    }
    for (m, v) in &turn.messages {
        insert_message(&mut tx, id, m, *v).await?;
    }
    if let Some((version, text)) = turn.plan {
        sqlx::query("INSERT OR IGNORE INTO plan_versions (item_id, version, text, proposed_at) VALUES (?1, ?2, ?3, ?4)")
            .bind(id)
            .bind(version)
            .bind(text)
            .bind(&now)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(true)
}

/// Every accepted plan version of item `id`, oldest first.
pub async fn plan_versions(pool: &SqlitePool, id: i64) -> Result<Vec<PlanVersion>> {
    Ok(sqlx::query_as("SELECT version, text, proposed_at AS at FROM plan_versions WHERE item_id = ?1 ORDER BY version")
        .bind(id)
        .fetch_all(pool)
        .await?)
}

pub async fn messages(pool: &SqlitePool, id: i64) -> Result<Vec<ItemMessage>> {
    Ok(sqlx::query_as(
        "SELECT id, item_id, at, author, via, body, pending_agent, read_by_task, wa_state, notice, plan_version
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

/// Record that nothing is sent for thread message `message_id`.
pub async fn set_wa_none(pool: &SqlitePool, message_id: i64) -> Result<()> {
    sqlx::query("UPDATE item_messages SET wa_state = 'none' WHERE id = ?1 AND wa_state IS NULL")
        .bind(message_id)
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

// ── operator messages from WhatsApp ──────────────────────────────────────

/// Processing state of one operator message (`inbound_commands`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundState {
    /// `received`, `applied` or `failed`.
    pub state: String,
    pub attempts: i64,
}

impl InboundState {
    /// Applied, or failed for good: the watermark may move past it.
    pub fn is_final(&self) -> bool {
        self.state == "applied" || self.state == "failed"
    }
}

/// Record that operator message `msg_ref` (WhatsApp row `row_id`) was read;
/// returns its state (a message read before keeps its state).
pub async fn inbound_receive(pool: &SqlitePool, msg_ref: &str, row_id: i64, item_key: &str) -> Result<InboundState> {
    let now = crate::timestamp::now();
    sqlx::query(
        "INSERT OR IGNORE INTO inbound_commands (msg_ref, wa_row_id, item_key, state, attempts, received_at, updated_at)
         VALUES (?1, ?2, ?3, 'received', 0, ?4, ?4)",
    )
    .bind(msg_ref)
    .bind(row_id)
    .bind(item_key)
    .bind(&now)
    .execute(pool)
    .await?;
    inbound_state(pool, msg_ref).await?.context("inbound message vanished")
}

pub async fn inbound_state(pool: &SqlitePool, msg_ref: &str) -> Result<Option<InboundState>> {
    let row: Option<(String, i64)> = sqlx::query_as("SELECT state, attempts FROM inbound_commands WHERE msg_ref = ?1")
        .bind(msg_ref)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|(state, attempts)| InboundState { state, attempts }))
}

/// Finish operator message `msg_ref` as `applied` or `failed` (a refused
/// command, or one that failed `max` times).
pub async fn inbound_finish(pool: &SqlitePool, msg_ref: &str, state: &str, error: Option<&str>) -> Result<()> {
    if !matches!(state, "applied" | "failed") {
        bail!("an inbound message ends applied or failed, not {state}");
    }
    let now = crate::timestamp::now();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("UPDATE inbound_commands SET state = ?2, error = ?3, updated_at = ?4 WHERE msg_ref = ?1 AND state = 'received'")
        .bind(msg_ref)
        .bind(state)
        .bind(error)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE confirmations SET state = ?2, closed_at = ?3 WHERE answer_ref = ?1 AND state = 'pending'")
        .bind(msg_ref)
        .bind(if state == "applied" { "confirmed" } else { "refused" })
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

// ── confirmations ────────────────────────────────────────────────────────

/// A confirmation question (see [`SCHEMA_V4`]).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Confirmation {
    pub id: i64,
    pub scope: String,
    pub item_id: i64,
    pub decision: String,
    pub plan_version: Option<i64>,
    pub hold_hash: Option<String>,
    pub question: String,
    pub asked_by: String,
    pub answer_ref: Option<String>,
    pub state: String,
    pub created_at: String,
    pub expires_at: String,
}

const CONFIRMATION_COLUMNS: &str =
    "id, scope, item_id, decision, plan_version, hold_hash, question, asked_by, answer_ref, state, created_at, expires_at";

/// What a new confirmation binds to.
pub struct NewConfirmation<'a> {
    pub scope: &'a str,
    pub item_id: i64,
    pub decision: &'a str,
    pub plan_version: Option<i64>,
    pub hold_hash: Option<&'a str>,
    pub question: &'a str,
    /// The operator message the question answers.
    pub asked_by: &'a str,
    pub expires_at: &'a str,
}

/// Ask a confirmation: an earlier open question in the same scope is
/// replaced, the new one is stored, and the operator message that led to
/// it is marked applied, in one transaction.
pub async fn ask_confirmation(pool: &SqlitePool, c: &NewConfirmation<'_>) -> Result<i64> {
    let now = crate::timestamp::now();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("UPDATE confirmations SET state = 'replaced', closed_at = ?2 WHERE scope = ?1 AND state = 'pending'")
        .bind(c.scope)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    let id = sqlx::query(
        "INSERT INTO confirmations (scope, item_id, decision, plan_version, hold_hash, question, asked_by, state, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8, ?9)",
    )
    .bind(c.scope)
    .bind(c.item_id)
    .bind(c.decision)
    .bind(c.plan_version)
    .bind(c.hold_hash)
    .bind(c.question)
    .bind(c.asked_by)
    .bind(&now)
    .bind(c.expires_at)
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();
    mark_applied(&mut tx, Some(c.asked_by)).await?;
    tx.commit().await?;
    Ok(id)
}

/// The open question in `scope` that operator message `msg_ref` may answer:
/// pending, not expired at `now`, and not being answered by another
/// message.
pub async fn open_confirmation(pool: &SqlitePool, scope: &str, msg_ref: &str, now: &str) -> Result<Option<Confirmation>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {CONFIRMATION_COLUMNS} FROM confirmations
          WHERE scope = ?1 AND state = 'pending' AND expires_at > ?3 AND (answer_ref IS NULL OR answer_ref = ?2)
          ORDER BY id DESC LIMIT 1"
    ))
    .bind(scope)
    .bind(msg_ref)
    .bind(now)
    .fetch_optional(pool)
    .await?)
}

/// Pending questions in `scope` that expired before `now`: marked
/// `expired` and returned (the operator is told when he answers one late).
pub async fn expire_confirmations(pool: &SqlitePool, scope: &str, now: &str) -> Result<Vec<Confirmation>> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let rows: Vec<Confirmation> = sqlx::query_as(&format!(
        "SELECT {CONFIRMATION_COLUMNS} FROM confirmations WHERE scope = ?1 AND state = 'pending' AND expires_at <= ?2 ORDER BY id"
    ))
    .bind(scope)
    .bind(now)
    .fetch_all(&mut *tx)
    .await?;
    sqlx::query("UPDATE confirmations SET state = 'expired', closed_at = ?2 WHERE scope = ?1 AND state = 'pending' AND expires_at <= ?2")
        .bind(scope)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(rows)
}

/// Record that operator message `msg_ref` is being applied as the answer
/// to confirmation `id`. False when the question is no longer open for it.
pub async fn claim_confirmation(pool: &SqlitePool, id: i64, msg_ref: &str) -> Result<bool> {
    let res = sqlx::query(
        "UPDATE confirmations SET answer_ref = ?2 WHERE id = ?1 AND state = 'pending' AND (answer_ref IS NULL OR answer_ref = ?2)",
    )
    .bind(id)
    .bind(msg_ref)
    .execute(pool)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// The operator answered no: the question is declined and his message
/// applied, in one transaction.
pub async fn decline_confirmation(pool: &SqlitePool, id: i64, msg_ref: &str) -> Result<()> {
    let now = crate::timestamp::now();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("UPDATE confirmations SET state = 'declined', answer_ref = ?2, closed_at = ?3 WHERE id = ?1 AND state = 'pending'")
        .bind(id)
        .bind(msg_ref)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    mark_applied(&mut tx, Some(msg_ref)).await?;
    tx.commit().await?;
    Ok(())
}

/// The question was asked again (an answer to it could not be verified):
/// operator message `msg_ref`, whose reply carried the question again, is
/// now the one the next answer is checked against, and it is applied, in
/// one transaction. The question stays open with its expiry.
pub async fn reask_confirmation(pool: &SqlitePool, id: i64, msg_ref: &str) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query("UPDATE confirmations SET asked_by = ?2 WHERE id = ?1 AND state = 'pending'")
        .bind(id)
        .bind(msg_ref)
        .execute(&mut *tx)
        .await?;
    mark_applied(&mut tx, Some(msg_ref)).await?;
    tx.commit().await?;
    Ok(())
}

/// Another message came instead of an answer: the open questions in
/// `scope` are replaced.
pub async fn replace_confirmations(pool: &SqlitePool, scope: &str) -> Result<()> {
    sqlx::query("UPDATE confirmations SET state = 'replaced', closed_at = ?2 WHERE scope = ?1 AND state = 'pending' AND answer_ref IS NULL")
        .bind(scope)
        .bind(crate::timestamp::now())
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn confirmation(pool: &SqlitePool, id: i64) -> Result<Confirmation> {
    sqlx::query_as(&format!("SELECT {CONFIRMATION_COLUMNS} FROM confirmations WHERE id = ?1"))
        .bind(id)
        .fetch_optional(pool)
        .await?
        .with_context(|| format!("no confirmation {id}"))
}

/// Count a failed attempt at applying `msg_ref`; returns the new state.
pub async fn inbound_attempt_failed(pool: &SqlitePool, msg_ref: &str, error: &str) -> Result<InboundState> {
    sqlx::query("UPDATE inbound_commands SET attempts = attempts + 1, error = ?2, updated_at = ?3 WHERE msg_ref = ?1")
        .bind(msg_ref)
        .bind(error)
        .bind(crate::timestamp::now())
        .execute(pool)
        .await?;
    inbound_state(pool, msg_ref).await?.context("inbound message vanished")
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
            notice: None,
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
    async fn the_review_stage_is_migrated_away() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        // A database at version 2, as the deployed code left it.
        let pool = crate::db::open(&dir.path().join(super::super::INTAKE_DB_PATH)).await.unwrap();
        crate::migrate::migrate(
            &pool,
            &[
                crate::migrate::Migration { version: 1, name: "intake schema", step: crate::migrate::Step::Sql(SCHEMA_V1) },
                crate::migrate::Migration { version: 2, name: "hidden-content hold", step: crate::migrate::Step::Sql(SCHEMA_V2) },
            ],
        )
        .await
        .unwrap();
        let rows = [
            // (stage, comment_state, failed_stage, comment_op)
            ("review", "proposed", None, None),
            ("review", "approved", None, Some("op1")),
            ("review", "posted", None, None),
            ("review", "skipped", None, None),
            ("failed", "none", Some("review"), None),
            ("blocked", "approved", Some("review"), None),
            ("closed", "posted", None, None),
        ];
        for (n, (stage, cs, failed, op)) in rows.into_iter().enumerate() {
            // One event per item: an event has at most one open item.
            let ev = sqlx::query(
                "INSERT INTO events (source, external_id, kind, title, body, labels_json, state, raw_json, accepted, first_seen_at, last_seen_at)
                 VALUES ('github', ?1, 'issue', 't', 'b', '[]', 'open', '{}', 1, 't', 't')",
            )
            .bind(format!("acme/widget#{n}"))
            .execute(&pool)
            .await
            .unwrap()
            .last_insert_rowid();
            sqlx::query(
                "INSERT INTO items (event_id, repo, title, stage, comment_state, comment_draft, failed_stage, comment_op, created_at, updated_at)
                 VALUES (?5, 'acme/widget', 't', ?1, ?2, 'draft text', ?3, ?4, 't', 't')",
            )
            .bind(stage)
            .bind(cs)
            .bind(failed)
            .bind(op)
            .bind(ev)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool.close().await;
        let pool = open(dir.path()).await.unwrap();
        assert!(schema_ready(&pool).await);
        type Row = (String, String, Option<String>, Option<String>, Option<String>);
        let got: Vec<Row> =
            sqlx::query_as("SELECT stage, comment_state, failed_stage, comment_op, closed_at FROM items ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        let brief: Vec<(&str, &str, Option<&str>, Option<&str>)> =
            got.iter().map(|r| (r.0.as_str(), r.1.as_str(), r.2.as_deref(), r.3.as_deref())).collect();
        assert_eq!(
            brief,
            [
                ("pr", "none", None, None),
                ("pr", "none", None, Some("op1")),
                ("closed", "posted", None, None),
                ("closed", "skipped", None, None),
                ("failed", "none", Some("pr"), None),
                ("blocked", "none", Some("pr"), None),
                ("closed", "posted", None, None),
            ]
        );
        assert!(got[2].4.is_some() && got[3].4.is_some(), "closed rows have closed_at");
        for id in 1..=7 {
            item(&pool, id).await.expect("every migrated row reads as an Item");
        }
        let cols: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('items')").fetch_all(&pool).await.unwrap();
        assert!(!cols.iter().any(|c| c == "comment_draft"));
        let log: Vec<(i64, String, String)> =
            sqlx::query_as("SELECT item_id, from_stage, to_stage FROM item_transitions ORDER BY item_id").fetch_all(&pool).await.unwrap();
        assert_eq!(
            log,
            [(1, "review".into(), "pr".into()), (2, "review".into(), "pr".into()), (3, "review".into(), "closed".into()), (4, "review".into(), "closed".into())]
        );
        // A retry of the blocked item resumes in pr.
        let it = item(&pool, 6).await.unwrap();
        assert!(advance(&pool, 6, it.stage(), StageEvent::Retry { failed_in: Stage::Pr }, "retry", vec![]).await.unwrap());
        assert_eq!(item(&pool, 6).await.unwrap().stage(), Stage::Pr);
    }

    /// A database at version 4, as the deployed code left it.
    pub(crate) async fn db_at_v4(dir: &Path) -> SqlitePool {
        std::fs::create_dir_all(dir.join("memory")).unwrap();
        let pool = crate::db::open(&dir.join(super::super::INTAKE_DB_PATH)).await.unwrap();
        crate::migrate::migrate(
            &pool,
            &[
                crate::migrate::Migration { version: 1, name: "intake schema", step: crate::migrate::Step::Sql(SCHEMA_V1) },
                crate::migrate::Migration { version: 2, name: "hidden-content hold", step: crate::migrate::Step::Sql(SCHEMA_V2) },
                crate::migrate::Migration { version: 3, name: "no comment approval", step: crate::migrate::Step::Sql(SCHEMA_V3) },
                crate::migrate::Migration { version: 4, name: "operator confirmations", step: crate::migrate::Step::Sql(SCHEMA_V4) },
            ],
        )
        .await
        .unwrap();
        pool
    }

    #[tokio::test]
    async fn the_group_surfaces_are_migrated_to_the_dm() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db_at_v4(dir.path()).await;
        // (surface, group_jid, stage): a cancelled item whose thread ran in a
        // group, one waiting for its group, one in the DM, one not on
        // WhatsApp yet.
        let rows = [("group", Some("g1"), "cancelled"), ("pending", None, "refinement"), ("dm", None, "refinement"), ("none", None, "eval")];
        for (n, (surface, jid, stage)) in rows.into_iter().enumerate() {
            let ev = sqlx::query(
                "INSERT INTO events (source, external_id, kind, title, body, labels_json, state, raw_json, accepted, first_seen_at, last_seen_at)
                 VALUES ('github', ?1, 'issue', 't', 'b', '[]', 'open', '{}', 1, 't', 't')",
            )
            .bind(format!("acme/widget#{n}"))
            .execute(&pool)
            .await
            .unwrap()
            .last_insert_rowid();
            sqlx::query(
                "INSERT INTO items (event_id, repo, title, stage, surface, group_jid, group_requested_at, created_at, updated_at)
                 VALUES (?1, 'acme/widget', 't', ?2, ?3, ?4, CASE WHEN ?3 IN ('group', 'pending') THEN 't' END, 't', 't')",
            )
            .bind(ev)
            .bind(stage)
            .bind(surface)
            .bind(jid)
            .execute(&pool)
            .await
            .unwrap();
        }
        for (scope, state) in [("group:1", "pending"), ("dm", "pending"), ("group:2", "declined")] {
            sqlx::query(
                "INSERT INTO confirmations (scope, item_id, decision, question, asked_by, state, created_at, expires_at)
                 VALUES (?1, 2, 'cancel', 'q', 'wa:x', ?2, 't', '9999')",
            )
            .bind(scope)
            .bind(state)
            .execute(&pool)
            .await
            .unwrap();
        }
        pool.close().await;
        let pool = open(dir.path()).await.unwrap();
        assert!(schema_ready(&pool).await);
        let surfaces: Vec<String> = sqlx::query_scalar("SELECT surface FROM items ORDER BY id").fetch_all(&pool).await.unwrap();
        assert_eq!(surfaces, ["dm", "dm", "dm", "none"]);
        let cols: Vec<String> = sqlx::query_scalar("SELECT name FROM pragma_table_info('items')").fetch_all(&pool).await.unwrap();
        assert!(!cols.iter().any(|c| c.starts_with("group_")), "{cols:?}");
        let states: Vec<String> = sqlx::query_scalar("SELECT state FROM confirmations ORDER BY id").fetch_all(&pool).await.unwrap();
        assert_eq!(states, ["replaced", "pending", "declined"], "an open question in a group scope is closed");
        for id in 1..=4 {
            item(&pool, id).await.expect("every migrated row reads as an Item");
        }
    }

    #[tokio::test]
    async fn the_migrations_keep_every_known_plan_and_move_a_group_item_to_the_dm() {
        let dir = tempfile::tempdir().unwrap();
        let pool = db_at_v4(dir.path()).await;
        let ev = |n: i64| {
            let pool = pool.clone();
            async move {
                sqlx::query(
                    "INSERT INTO events (source, external_id, kind, title, body, labels_json, state, raw_json, accepted, first_seen_at, last_seen_at)
                     VALUES ('github', ?1, 'issue', 't', 'b', '[]', 'open', '{}', 1, 't', 't')",
                )
                .bind(format!("acme/widget#{n}"))
                .execute(&pool)
                .await
                .unwrap()
                .last_insert_rowid()
            }
        };
        // Item 1: in refinement in a group, plan v2 proposed; its thread holds
        // the replies that carried v1 and v2, and an unsent message.
        let e1 = ev(1).await;
        sqlx::query(
            "INSERT INTO items (event_id, repo, title, stage, surface, group_jid, plan_draft, plan_version, created_at, updated_at)
             VALUES (?1, 'acme/widget', 't', 'refinement', 'group', 'g1', 'the whole plan v2', 2, 't', '2026-09-25T10:00:00.000Z')",
        )
        .bind(e1)
        .execute(&pool)
        .await
        .unwrap();
        for (at, body, wa) in [
            ("2026-09-24T10:00:00.000Z", "Idea.\n── plan v1 ──\nthe whole plan v1\n── end of plan v1 ──\nOK?", Some("queued")),
            ("2026-09-24T11:00:00.000Z", "Better.\n── plan v2 ──\nthe whole plan v2\n── end of plan v2 ──", None),
        ] {
            sqlx::query("INSERT INTO item_messages (item_id, at, author, via, body, wa_state) VALUES (1, ?1, 'agent', 'pipeline', ?2, ?3)")
                .bind(at)
                .bind(body)
                .bind(wa)
                .execute(&pool)
                .await
                .unwrap();
        }
        // Item 2: cancelled after its plan v1 was approved; no reply is left
        // to recover it from.
        let e2 = ev(2).await;
        sqlx::query(
            "INSERT INTO items (event_id, repo, title, stage, surface, plan_draft, plan_version, approved_plan, approved_version,
                                approved_at, created_at, updated_at)
             VALUES (?1, 'acme/widget', 't', 'cancelled', 'dm', 'approved text', 1, 'approved text', 1, '2026-09-23T09:00:00.000Z', 't', 't')",
        )
        .bind(e2)
        .execute(&pool)
        .await
        .unwrap();
        pool.close().await;

        let pool = open(dir.path()).await.unwrap();
        assert!(schema_ready(&pool).await);
        let it = item(&pool, 1).await.unwrap();
        assert_eq!((it.surface.as_str(), it.plan_refused_chars, it.plan_refusals), ("dm", None, 0));
        let v1 = plan_versions(&pool, 1).await.unwrap();
        assert_eq!(
            v1,
            [
                PlanVersion { version: 1, text: "the whole plan v1".into(), at: "2026-09-24T10:00:00.000Z".into() },
                PlanVersion { version: 2, text: "the whole plan v2".into(), at: "2026-09-24T11:00:00.000Z".into() },
            ]
        );
        let msgs = messages(&pool, 1).await.unwrap();
        assert_eq!(msgs.iter().map(|m| m.plan_version).collect::<Vec<_>>(), [Some(1), Some(2)]);
        assert_eq!(msgs[1].wa_state.as_deref(), Some("none"), "an unsent message is not sent in full after the change");
        let v2 = plan_versions(&pool, 2).await.unwrap();
        assert_eq!(v2, [PlanVersion { version: 1, text: "approved text".into(), at: "2026-09-23T09:00:00.000Z".into() }]);
        // Running the plan step again changes nothing.
        schema_v7(&pool).await.unwrap();
        assert_eq!(plan_versions(&pool, 1).await.unwrap(), v1);
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
