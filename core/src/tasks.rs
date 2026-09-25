//! Task ledger and background workers (ADR-033).
//!
//! A task is one row in `memory/tasks.db` plus one worker: a detached
//! `nucleus tasks run <id>` process that drives a one-shot Claude session in
//! its own window of the `nucleus-tasks` tmux session until the session's
//! work is complete, then records the result and delivers it to the task's
//! origin.
//!
//! The ledger is venue-agnostic. `origin` says where a task came from and
//! where its result goes (`whatsapp-dm`, `discord-home`, `cli`, `dashboard`,
//! `pipeline`); `origin_ref` names the exact chat for `whatsapp-dm`; `kind`,
//! `parent_id` and `task_links` let later producers (the issue pipeline)
//! chain tasks and attach external references without schema changes.
//!
//! **Write ownership (ADR-020, as clarified there).** Every write to tasks.db
//! goes through this module, and this module only runs inside the `nucleus`
//! binary: the `nucleus tasks` subcommands, the detached `nucleus tasks run`
//! worker, and the dashboard's cancel endpoint. These are overlapping
//! invocations of one program, so every lifecycle transition is one
//! `BEGIN IMMEDIATE` transaction whose `WHERE` clause re-checks the state it
//! transitions from: two writers can never both finish, cancel or reap the
//! same task. The WhatsApp bot never opens tasks.db; it runs
//! `nucleus tasks sweep`. The dashboard's read paths use [`open_read_only`].
//!
//! **Lifecycle.** `queued` → `running` → `done` | `failed` | `cancelled`;
//! `interrupted` when the worker process disappeared (machine restart, crash)
//! without finishing. There is no automatic resume: an interrupted task is
//! reported to its origin once, and the operator decides. A worker keeps
//! running across WhatsApp bot restarts (it is a separate process in its own
//! tmux session).
//!
//! **Delivery** is at-least-once and idempotent: a finished task is claimed
//! for delivery, its WhatsApp rows are inserted in one transaction with
//! unique keys, and [`sweep`] retries every finished task whose delivery did
//! not complete. `delivered_at` is set only when the destination has the
//! message: when the WhatsApp bot marked the queued outbound row `sent`
//! (a row that failed for good is queued again under a new key), or when
//! Discord returned the message, sent through the `task_outbox` row that
//! is written before the send.
//!
//! **Access.** [`Scope`] limits what a caller sees: the operator sees every
//! task, a WhatsApp chat session only the tasks its chat started (see
//! `crate::caller`).

use crate::claude_session::{infra_failure_description, Session};
use crate::config::{Settings, TasksConfig};
use crate::session_profile::{ProfileContext, SessionProfile};
use crate::turn_tracker::{TrackEvent, TurnTracker};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Relative to the workspace root.
pub const TASKS_DB_PATH: &str = "memory/tasks.db";
/// Default tmux session that hosts every worker window (`[tasks]
/// tmux_session` overrides it).
pub const TASKS_TMUX_SESSION: &str = "nucleus-tasks";
/// Registry agent name (agents.toml) and run-log label.
pub const TASKS_AGENT_LABEL: &str = "tasks";

/// Where a task may come from. Result delivery is keyed on this value.
pub const ORIGINS: &[&str] = &["whatsapp-dm", "discord-home", "cli", "dashboard", "pipeline"];
/// Who asked for a task.
pub const REQUESTERS: &[&str] = &["model", "operator", "pipeline", "cli"];

/// Largest accepted brief. A brief is instructions, not data: anything larger
/// belongs in a file the brief names.
pub const MAX_BRIEF_CHARS: usize = 32_000;
pub const MAX_TITLE_CHARS: usize = 200;

/// A worker writes a heartbeat this often; `sweep` treats a task whose
/// heartbeat is older than [`STALE_AFTER`] and whose worker process is gone
/// as interrupted.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(30);
const STALE_AFTER: Duration = Duration::from_secs(180);
/// How often a queued worker re-checks for a free slot.
const CAP_POLL: Duration = Duration::from_secs(15);
/// A delivery claim older than this is considered abandoned (the deliverer
/// died) and may be taken again.
const DELIVERY_CLAIM_TTL: Duration = Duration::from_secs(300);
/// Deliveries are retried by `sweep` until this many attempts failed, and
/// only for tasks that finished within [`DELIVERY_RETRY_WINDOW`].
const DELIVERY_MAX_FAILURES: i64 = 5;
const DELIVERY_RETRY_WINDOW: Duration = Duration::from_secs(7 * 24 * 3600);
/// Largest result text copied into a WhatsApp message and into the chat
/// session. The ledger keeps the full text.
const DELIVERY_MAX_CHARS: usize = 12_000;
const INJECT_MAX_CHARS: usize = 6_000;
const DISCORD_MAX_CHARS: usize = 1_900;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
    Interrupted,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Interrupted => "interrupted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "queued" => Self::Queued,
            "running" => Self::Running,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            "interrupted" => Self::Interrupted,
            _ => return None,
        })
    }

    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Queued | Self::Running)
    }
}

/// One ledger row. `status` is the text form of [`TaskStatus`]; the
/// dashboard narrows it to a union.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export)]
pub struct Task {
    pub id: String,
    pub kind: String,
    pub title: String,
    pub brief: String,
    pub origin: String,
    /// Venue-specific pointer back to where the task was requested; for
    /// `whatsapp-dm`, the exact chat the result goes to.
    pub origin_ref: Option<String>,
    pub parent_id: Option<String>,
    /// `model`, `operator`, `pipeline` or `cli`.
    pub requested_by: String,
    pub status: String,
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub heartbeat_at: Option<String>,
    /// Unused since cancel became an immediate transition; kept because the
    /// v1 schema has the column.
    pub cancel_requested_at: Option<String>,
    #[ts(type = "number | null")]
    pub runner_pid: Option<i64>,
    pub session_id: Option<String>,
    pub tmux_window: Option<String>,
    pub transcript_path: Option<String>,
    pub result: Option<String>,
    pub error: Option<String>,
    pub delivered_at: Option<String>,
    /// Set while one process delivers the result; see [`deliver`].
    pub delivery_claimed_at: Option<String>,
    /// WhatsApp: the result's outbound row was queued at this time and the
    /// bot has not reported it sent yet. `delivered_at` is set when it is.
    pub delivery_queued_at: Option<String>,
    /// The delivery was given up at this time and is not attempted again:
    /// its outcome is unknown (the send failed after the message may have
    /// reached the destination), or it failed [`DELIVERY_MAX_FAILURES`]
    /// times. The operator gets one note. `delivered_at` can still be set
    /// later, when the bot learns the message did arrive.
    pub delivery_failed_at: Option<String>,
    /// Why the delivery was given up.
    pub delivery_error: Option<String>,
}

impl Task {
    pub fn short_id(&self) -> &str {
        &self.id[..8.min(self.id.len())]
    }

    pub fn status(&self) -> TaskStatus {
        TaskStatus::parse(&self.status).unwrap_or(TaskStatus::Failed)
    }
}

/// One progress-log entry.
#[derive(Debug, Clone, Serialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export)]
pub struct TaskEvent {
    #[ts(type = "number")]
    pub id: i64,
    pub task_id: String,
    pub at: String,
    /// created | queued_for_slot | started | progress | background |
    /// runtime_guard | done | failed | cancelled | interrupted |
    /// delivery_queued | delivered | delivery_failed | delivery_given_up |
    /// delivery_noted
    pub kind: String,
    pub message: String,
}

/// A reference from a task to something else: another task, an issue, a
/// pull request, a file. `rel` is free text (`blocks`, `issue`, `pr`, …).
#[derive(Debug, Clone, Serialize, sqlx::FromRow, ts_rs::TS)]
#[ts(export)]
pub struct TaskLink {
    pub task_id: String,
    pub rel: String,
    pub target: String,
    pub created_at: String,
}

/// Input for [`create`].
#[derive(Debug, Clone)]
pub struct NewTask {
    pub kind: String,
    pub title: String,
    pub brief: String,
    pub origin: String,
    pub origin_ref: Option<String>,
    pub parent_id: Option<String>,
    pub requested_by: String,
    pub links: Vec<(String, String)>,
}

/// Which tasks a caller may see and act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// Every task (a shell, the dashboard, an interactive session).
    Operator,
    /// Only tasks with this origin and origin_ref (a WhatsApp chat session).
    Origin { origin: String, origin_ref: String },
}

impl Scope {
    pub fn allows(&self, t: &Task) -> bool {
        match self {
            Scope::Operator => true,
            Scope::Origin { origin, origin_ref } => {
                &t.origin == origin && t.origin_ref.as_deref() == Some(origin_ref.as_str())
            }
        }
    }
}

// ── storage ──────────────────────────────────────────────────────────────

const SCHEMA_V1: &str = "
CREATE TABLE IF NOT EXISTS tasks (
    id                  TEXT PRIMARY KEY,
    kind                TEXT NOT NULL,
    title               TEXT NOT NULL,
    brief               TEXT NOT NULL,
    origin              TEXT NOT NULL,
    origin_ref          TEXT,
    parent_id           TEXT REFERENCES tasks(id),
    requested_by        TEXT NOT NULL,
    status              TEXT NOT NULL,
    created_at          TEXT NOT NULL,
    started_at          TEXT,
    finished_at         TEXT,
    heartbeat_at        TEXT,
    cancel_requested_at TEXT,
    runner_pid          INTEGER,
    session_id          TEXT,
    tmux_window         TEXT,
    transcript_path     TEXT,
    result              TEXT,
    error               TEXT,
    delivered_at        TEXT
);
CREATE INDEX IF NOT EXISTS idx_tasks_status_created ON tasks(status, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_tasks_parent ON tasks(parent_id);
CREATE TABLE IF NOT EXISTS task_events (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id TEXT NOT NULL REFERENCES tasks(id),
    at      TEXT NOT NULL,
    kind    TEXT NOT NULL,
    message TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_task_events_task ON task_events(task_id, id);
CREATE TABLE IF NOT EXISTS task_links (
    task_id    TEXT NOT NULL REFERENCES tasks(id),
    rel        TEXT NOT NULL,
    target     TEXT NOT NULL,
    created_at TEXT NOT NULL,
    PRIMARY KEY (task_id, rel, target)
)";

/// v2: one deliverer at a time ([`deliver`]), and scoped lookups by origin.
const SCHEMA_V2: &str = "
ALTER TABLE tasks ADD COLUMN delivery_claimed_at TEXT;
CREATE INDEX IF NOT EXISTS idx_tasks_origin ON tasks(origin, origin_ref, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_tasks_session ON tasks(session_id)";

/// v3: the worker's one-time run token; WhatsApp delivery confirmed by the
/// outbound row's status; the Discord outbox.
const SCHEMA_V3: &str = "
ALTER TABLE tasks ADD COLUMN run_token_sha256 TEXT;
ALTER TABLE tasks ADD COLUMN delivery_queued_at TEXT;
ALTER TABLE tasks ADD COLUMN delivery_ref INTEGER;
ALTER TABLE tasks ADD COLUMN delivery_redrives INTEGER NOT NULL DEFAULT 0;
CREATE TABLE IF NOT EXISTS task_outbox (
    key        TEXT PRIMARY KEY,
    task_id    TEXT NOT NULL REFERENCES tasks(id),
    channel    TEXT NOT NULL,
    body       TEXT NOT NULL,
    status     TEXT NOT NULL,
    message_id TEXT,
    attempts   INTEGER NOT NULL DEFAULT 0,
    last_error TEXT,
    created_at TEXT NOT NULL,
    sent_at    TEXT
)";

/// v4: a delivery given up (its outcome is unknown, or it failed too often)
/// is recorded and never sent again; the operator gets one note about it.
const SCHEMA_V4: &str = "
ALTER TABLE tasks ADD COLUMN delivery_failed_at TEXT;
ALTER TABLE tasks ADD COLUMN delivery_error TEXT;
ALTER TABLE tasks ADD COLUMN delivery_noted_at TEXT";

/// The columns of [`Task`], in order. Reads name them instead of `SELECT *`:
/// a connection that prepared a statement before a migration added a column
/// would otherwise see a different column count than the row it steps.
const TASK_COLUMNS: &str = "id, kind, title, brief, origin, origin_ref, parent_id, requested_by, \
    status, created_at, started_at, finished_at, heartbeat_at, cancel_requested_at, runner_pid, \
    session_id, tmux_window, transcript_path, result, error, delivered_at, delivery_claimed_at, \
    delivery_queued_at, delivery_failed_at, delivery_error";

/// Open (creating and migrating) tasks.db. Writers only.
pub async fn open(workspace_root: &Path) -> Result<SqlitePool> {
    let pool = crate::db::open(&workspace_root.join(TASKS_DB_PATH)).await?;
    crate::migrate::migrate(
        &pool,
        &[
            crate::migrate::Migration {
                version: 1,
                name: "baseline tasks ledger",
                step: crate::migrate::Step::Sql(SCHEMA_V1),
            },
            crate::migrate::Migration {
                version: 2,
                name: "delivery claim, origin and session indexes",
                step: crate::migrate::Step::Sql(SCHEMA_V2),
            },
            crate::migrate::Migration {
                version: 3,
                name: "run token, confirmed whatsapp delivery, discord outbox",
                step: crate::migrate::Step::Sql(SCHEMA_V3),
            },
            crate::migrate::Migration {
                version: 4,
                name: "delivery given up, operator note",
                step: crate::migrate::Step::Sql(SCHEMA_V4),
            },
        ],
    )
    .await
    .context("migrating tasks.db")?;
    Ok(pool)
}

/// Open an existing tasks.db read-only (dashboard reads).
pub async fn open_read_only(workspace_root: &Path) -> Result<SqlitePool> {
    crate::db::open_read_only(&workspace_root.join(TASKS_DB_PATH)).await
}

fn ago(d: Duration) -> String {
    let t = chrono::Utc::now() - chrono::Duration::from_std(d).unwrap_or_default();
    t.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

async fn add_event_on(
    conn: &mut sqlx::SqliteConnection,
    task_id: &str,
    kind: &str,
    message: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO task_events (task_id, at, kind, message) VALUES (?1, ?2, ?3, ?4)")
        .bind(task_id)
        .bind(crate::timestamp::now())
        .bind(kind)
        .bind(message)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub async fn add_event(pool: &SqlitePool, task_id: &str, kind: &str, message: &str) -> Result<()> {
    let mut conn = pool.acquire().await?;
    add_event_on(&mut conn, task_id, kind, message).await
}

/// Validate and canonicalize a new task. Pure except the DM allowlist read
/// for a `whatsapp-dm` origin_ref.
fn validate(t: &mut NewTask) -> Result<()> {
    if !ORIGINS.contains(&t.origin.as_str()) {
        bail!("unknown origin {:?} (expected one of: {})", t.origin, ORIGINS.join(", "));
    }
    if !REQUESTERS.contains(&t.requested_by.as_str()) {
        bail!(
            "unknown requester {:?} (expected one of: {})",
            t.requested_by,
            REQUESTERS.join(", ")
        );
    }
    t.title = t.title.trim().to_string();
    t.brief = t.brief.trim().to_string();
    t.kind = t.kind.trim().to_string();
    if t.title.is_empty() {
        bail!("a task needs a title");
    }
    if t.title.chars().count() > MAX_TITLE_CHARS {
        bail!("the title is longer than {MAX_TITLE_CHARS} characters");
    }
    if t.brief.is_empty() {
        bail!("a task needs a brief");
    }
    let n = t.brief.chars().count();
    if n > MAX_BRIEF_CHARS {
        bail!(
            "the brief has {n} characters; the limit is {MAX_BRIEF_CHARS}. Put large inputs in a \
             file and name the file in the brief"
        );
    }
    if t.kind.is_empty() {
        t.kind = "general".into();
    }
    t.origin_ref = match (t.origin.as_str(), t.origin_ref.take()) {
        ("whatsapp-dm", Some(r)) => Some(crate::whatsapp_queue::canonical_dm_chat(&r)?),
        (_, r) => r.map(|r| r.trim().to_string()).filter(|r| !r.is_empty()),
    };
    Ok(())
}

/// Insert a new task in `queued` state, with its links and `created` event,
/// in one transaction. `scope` is the caller's: a parent task is looked up
/// among the tasks the caller may see. Does not start a worker; see
/// [`launch_worker`].
pub async fn create(pool: &SqlitePool, mut t: NewTask, scope: &Scope) -> Result<Task> {
    validate(&mut t)?;
    let parent = match &t.parent_id {
        Some(p) => Some(get(pool, p, scope).await.context("resolving --parent")?.id),
        None => None,
    };
    let id = uuid::Uuid::new_v4().simple().to_string();
    let now = crate::timestamp::now();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    sqlx::query(
        "INSERT INTO tasks (id, kind, title, brief, origin, origin_ref, parent_id, requested_by,
                            status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'queued', ?9)",
    )
    .bind(&id)
    .bind(&t.kind)
    .bind(&t.title)
    .bind(&t.brief)
    .bind(&t.origin)
    .bind(&t.origin_ref)
    .bind(&parent)
    .bind(&t.requested_by)
    .bind(&now)
    .execute(&mut *tx)
    .await?;
    for (rel, target) in &t.links {
        sqlx::query(
            "INSERT OR IGNORE INTO task_links (task_id, rel, target, created_at)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(&id)
        .bind(rel)
        .bind(target)
        .bind(&now)
        .execute(&mut *tx)
        .await?;
    }
    add_event_on(&mut tx, &id, "created", &format!("requested by {} via {}", t.requested_by, t.origin))
        .await?;
    tx.commit().await?;
    get(pool, &id, &Scope::Operator).await
}

/// Look a task up by full id or by a unique prefix of at least 4 characters,
/// among the tasks `scope` may see. A task outside the scope does not exist
/// for the caller.
pub async fn get(pool: &SqlitePool, id_or_prefix: &str, scope: &Scope) -> Result<Task> {
    let key = id_or_prefix.trim().trim_start_matches('#').to_lowercase();
    if key.len() < 4 || !key.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("task id {id_or_prefix:?} is not valid (use at least 4 characters of the id)");
    }
    let rows: Vec<Task> = sqlx::query_as(&format!("SELECT {TASK_COLUMNS} FROM tasks WHERE id LIKE ?1 || '%'"))
        .bind(&key)
        .fetch_all(pool)
        .await?;
    let rows: Vec<Task> = rows.into_iter().filter(|t| scope.allows(t)).collect();
    match rows.len() {
        0 => bail!("no task with id {id_or_prefix:?}"),
        1 => Ok(rows.into_iter().next().unwrap()),
        _ => bail!("task id prefix {id_or_prefix:?} matches more than one task"),
    }
}

/// Newest first, among the tasks `scope` may see. `active_only` keeps queued
/// and running tasks.
pub async fn list(pool: &SqlitePool, active_only: bool, limit: i64, scope: &Scope) -> Result<Vec<Task>> {
    let status = if active_only { "AND status IN ('queued','running')" } else { "" };
    let rows: Vec<Task> = match scope {
        Scope::Operator => {
            sqlx::query_as(&format!(
                "SELECT {TASK_COLUMNS} FROM tasks WHERE 1=1 {status} ORDER BY created_at DESC LIMIT ?1"
            ))
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
        Scope::Origin { origin, origin_ref } => {
            sqlx::query_as(&format!(
                "SELECT {TASK_COLUMNS} FROM tasks WHERE origin = ?2 AND origin_ref = ?3 {status}
                  ORDER BY created_at DESC LIMIT ?1"
            ))
            .bind(limit)
            .bind(origin)
            .bind(origin_ref)
            .fetch_all(pool)
            .await?
        }
    };
    Ok(rows)
}

pub async fn events(pool: &SqlitePool, task_id: &str) -> Result<Vec<TaskEvent>> {
    Ok(sqlx::query_as("SELECT * FROM task_events WHERE task_id = ?1 ORDER BY id")
        .bind(task_id)
        .fetch_all(pool)
        .await?)
}

pub async fn links(pool: &SqlitePool, task_id: &str) -> Result<Vec<TaskLink>> {
    Ok(sqlx::query_as("SELECT * FROM task_links WHERE task_id = ?1 ORDER BY created_at")
        .bind(task_id)
        .fetch_all(pool)
        .await?)
}

/// Terminal transition plus its event, in one transaction. Only moves a
/// non-terminal row; returns false when another writer finished the task
/// first.
async fn finish(
    pool: &SqlitePool,
    id: &str,
    status: TaskStatus,
    result: Option<&str>,
    error: Option<&str>,
) -> Result<bool> {
    finish_if(pool, id, status, result, error, None).await
}

/// [`finish`], additionally requiring the task's last sign of life
/// (`COALESCE(heartbeat_at, created_at)`) to still equal `last_seen`: a sweep
/// that read a stale heartbeat does not interrupt a worker that wrote a new
/// one in between.
async fn finish_if(
    pool: &SqlitePool,
    id: &str,
    status: TaskStatus,
    result: Option<&str>,
    error: Option<&str>,
    last_seen: Option<&str>,
) -> Result<bool> {
    debug_assert!(status.is_terminal());
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let done = sqlx::query(
        "UPDATE tasks SET status = ?2, finished_at = ?3, result = COALESCE(?4, result),
                          error = COALESCE(?5, error)
          WHERE id = ?1 AND status IN ('queued','running')
            AND (?6 IS NULL OR COALESCE(heartbeat_at, created_at) = ?6)",
    )
    .bind(id)
    .bind(status.as_str())
    .bind(crate::timestamp::now())
    .bind(result)
    .bind(error)
    .bind(last_seen)
    .execute(&mut *tx)
    .await?
    .rows_affected()
        == 1;
    if done {
        let msg = match (error, result) {
            (Some(e), _) => e.to_string(),
            (None, Some(r)) => format!("{} characters of result", r.chars().count()),
            _ => String::new(),
        };
        add_event_on(&mut tx, id, status.as_str(), &msg).await?;
    }
    tx.commit().await?;
    Ok(done)
}

/// Stop a task: the transition to `cancelled` happens here, at once and
/// atomically, for a queued or a running task; then the task's session
/// windows are killed and the origin is told. A running worker sees the
/// terminal status within a second and exits without writing.
pub async fn request_cancel(
    workspace_root: &Path,
    pool: &SqlitePool,
    cfg: &TasksConfig,
    id: &str,
    scope: &Scope,
) -> Result<Task> {
    let task = get(pool, id, scope).await?;
    if task.status().is_terminal() {
        bail!("task {} is already {}", task.short_id(), task.status);
    }
    let reason = match task.status() {
        TaskStatus::Queued => "cancelled before it started",
        _ => "cancelled",
    };
    if !finish(pool, &task.id, TaskStatus::Cancelled, None, Some(reason)).await? {
        let now = get(pool, &task.id, &Scope::Operator).await?;
        bail!("task {} is already {}", now.short_id(), now.status);
    }
    kill_task_windows(&cfg.tmux_session, &task).await;
    let t = get(pool, &task.id, &Scope::Operator).await?;
    deliver(workspace_root, pool, cfg, &t).await;
    Ok(t)
}

enum Claim {
    Claimed,
    NoSlot(i64),
    NotQueued(String),
}

/// Move `id` from queued to running if a slot is free. Serialized across
/// processes by `BEGIN IMMEDIATE`, so two workers cannot both take the last
/// slot.
async fn claim(pool: &SqlitePool, id: &str, pid: u32, cap: i64) -> Result<Claim> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let status: Option<String> = sqlx::query_scalar("SELECT status FROM tasks WHERE id = ?1")
        .bind(id)
        .fetch_optional(&mut *tx)
        .await?;
    let status = status.context("task disappeared")?;
    if status != "queued" {
        return Ok(Claim::NotQueued(status));
    }
    let running: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks WHERE status = 'running'")
        .fetch_one(&mut *tx)
        .await?;
    if running >= cap {
        return Ok(Claim::NoSlot(running));
    }
    let now = crate::timestamp::now();
    sqlx::query(
        "UPDATE tasks SET status = 'running', started_at = ?2, heartbeat_at = ?2, runner_pid = ?3
          WHERE id = ?1 AND status = 'queued'",
    )
    .bind(id)
    .bind(&now)
    .bind(pid as i64)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Claim::Claimed)
}

async fn heartbeat(pool: &SqlitePool, id: &str, pid: u32) -> Result<()> {
    sqlx::query(
        "UPDATE tasks SET heartbeat_at = ?2, runner_pid = ?3
          WHERE id = ?1 AND status IN ('queued','running')",
    )
    .bind(id)
    .bind(crate::timestamp::now())
    .bind(pid as i64)
    .execute(pool)
    .await?;
    Ok(())
}

fn age(stamp: &str) -> Option<Duration> {
    let t = chrono::DateTime::parse_from_rfc3339(stamp).ok()?;
    (chrono::Utc::now() - t.with_timezone(&chrono::Utc)).to_std().ok()
}

/// True when `pid` is alive and is the worker of `task_id`
/// (`nucleus tasks run <task_id>`). A reused pid belongs to another command
/// and does not count.
pub fn worker_alive(pid: Option<i64>, task_id: &str) -> bool {
    let Some(pid) = pid.filter(|p| *p > 0) else { return false };
    // SAFETY: kill with signal 0 only checks that the process exists.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
        return false;
    }
    std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .map(|o| {
            let cmd = String::from_utf8_lossy(&o.stdout);
            cmd.contains("tasks run") && cmd.contains(task_id)
        })
        .unwrap_or(false)
}

/// Kill every tmux window of `task`: the recorded window id, and any window
/// named `task-<short id>` in the tasks tmux session (a worker stopped while
/// its session was still booting had not recorded its window yet).
async fn kill_task_windows(tmux_session: &str, task: &Task) {
    use tokio::process::Command;
    let mut targets: Vec<String> = Vec::new();
    if let Some(w) = task.tmux_window.as_deref().filter(|w| is_window_id(w)) {
        targets.push(w.to_string());
    }
    let name = format!("task-{}", task.short_id());
    if let Ok(out) = Command::new("tmux")
        .args(["list-windows", "-t", tmux_session, "-F", "#{window_id} #{window_name}"])
        .output()
        .await
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if let Some((id, n)) = line.split_once(' ') {
                if n == name && is_window_id(id) && !targets.iter().any(|t| t == id) {
                    targets.push(id.to_string());
                }
            }
        }
    }
    for t in targets {
        let _ = Command::new("tmux").args(["kill-window", "-t", &t]).output().await;
    }
}

fn is_window_id(s: &str) -> bool {
    s.len() > 1 && s.starts_with('@') && s[1..].chars().all(|c| c.is_ascii_digit())
}

/// Mark tasks whose worker process is gone as interrupted, stop their
/// sessions, and report each one to its origin once; then settle queued
/// WhatsApp deliveries and retry every delivery that did not complete. A task is interrupted only when its
/// worker process is not running AND its last heartbeat is older than
/// [`STALE_AFTER`]; the transition re-checks that heartbeat, so a worker that
/// was only asleep (a closed laptop) and wrote a heartbeat meanwhile is left
/// alone. Returns the tasks it marked.
pub async fn sweep(workspace_root: &Path, pool: &SqlitePool, cfg: &TasksConfig) -> Result<Vec<Task>> {
    let candidates: Vec<Task> =
        sqlx::query_as(&format!("SELECT {TASK_COLUMNS} FROM tasks WHERE status IN ('queued','running')"))
            .fetch_all(pool)
            .await?;
    let mut marked = Vec::new();
    for t in candidates {
        let last = t.heartbeat_at.clone().unwrap_or_else(|| t.created_at.clone());
        let stale = age(&last).map(|a| a > STALE_AFTER).unwrap_or(true);
        if !stale || worker_alive(t.runner_pid, &t.id) {
            continue;
        }
        let reason = "the worker process stopped before the task finished (machine or process \
                      restart); not resumed";
        if finish_if(pool, &t.id, TaskStatus::Interrupted, None, Some(reason), Some(&last)).await? {
            kill_task_windows(&cfg.tmux_session, &t).await;
            let t = get(pool, &t.id, &Scope::Operator).await?;
            deliver(workspace_root, pool, cfg, &t).await;
            marked.push(t);
        }
    }
    if let Err(e) = confirm_whatsapp(workspace_root, pool).await {
        tracing::warn!(err = %format!("{e:#}"), "tasks: reading whatsapp delivery status failed");
    }
    retry_deliveries(workspace_root, pool, cfg).await?;
    if let Err(e) = note_given_up_deliveries(workspace_root, pool, cfg).await {
        tracing::warn!(err = %format!("{e:#}"), "tasks: noting given-up deliveries failed");
    }
    Ok(marked)
}

/// Deliver finished tasks whose delivery never completed (the deliverer died,
/// or the destination was unavailable).
async fn retry_deliveries(workspace_root: &Path, pool: &SqlitePool, cfg: &TasksConfig) -> Result<()> {
    let pending: Vec<Task> = sqlx::query_as(&format!(
        "SELECT {TASK_COLUMNS} FROM tasks t
          WHERE status IN ('done','failed','cancelled','interrupted')
            AND delivered_at IS NULL AND delivery_queued_at IS NULL AND delivery_failed_at IS NULL
            AND origin IN ('whatsapp-dm','discord-home')
            AND finished_at > ?1
            AND (SELECT COUNT(*) FROM task_events e
                  WHERE e.task_id = t.id AND e.kind = 'delivery_failed') < ?2"
    ))
    .bind(ago(DELIVERY_RETRY_WINDOW))
    .bind(DELIVERY_MAX_FAILURES)
    .fetch_all(pool)
    .await?;
    for t in pending {
        deliver(workspace_root, pool, cfg, &t).await;
    }
    Ok(())
}

// ── worker ───────────────────────────────────────────────────────────────

/// Code-owned instructions for every worker session.
const WORKER_PROMPT: &str = "\
You are a Nucleus background worker. You run one task, unattended. No one \
watches this session and no one will answer a question.

- Do the whole task described in the brief. Use the tools you need.
- Your final message is the task result. It is delivered to the operator as \
you write it and stored in the task ledger. Start with the result itself, \
with no preamble. Make it as long as the result needs and no longer. Write in \
the language of the brief.
- If something blocks you, finish what you can, then state what you finished, \
what blocked you, and what the operator must decide.
- Do not send messages to the operator or to any chat, and do not start, \
cancel or inspect background tasks: the tasks CLI and session-send refuse \
worker sessions. Delivery is automatic.
- Wait for every command you start to finish before you end your turn. If you \
run a command in the background, wait for its completion notice before you \
write the final message.";

/// Tool patterns a worker may not use, on top of the Settings denylist. The
/// CLIs also refuse worker callers (`crate::caller`); this list stops the
/// usual spellings before they run.
fn worker_denylist() -> Vec<String> {
    let mut out = Vec::new();
    for bin in ["./target/release/nucleus", "./target/debug/nucleus", "nucleus"] {
        out.push(format!("Bash({bin} tasks:*)"));
        out.push(format!("Bash({bin} session-send:*)"));
    }
    out
}

fn worker_message(task: &Task) -> String {
    format!(
        "[Nucleus background task {} — {:?}, kind {}, requested by {} via {}]\n\n{}",
        task.short_id(),
        task.title,
        task.kind,
        task.requested_by,
        task.origin,
        task.brief
    )
}

/// Start the detached worker process for `task_id`, and record its pid.
///
/// `nucleus tasks run <id>` is internal, and only the process started here
/// may run it: a one-time run token is generated, its SHA-256 is stored on
/// the queued task, and the token itself reaches the worker on its stdin —
/// not in its arguments or environment, which other processes of the same
/// user can read. The worker consumes the token when it takes the task
/// ([`consume_run_token`]); `tasks run` without it is refused.
///
/// The worker runs in its own session (`setsid`), so it survives the process
/// that started it — a chat session's Bash tool call, the dashboard, a
/// pipeline step, a restart of the WhatsApp bot. It does not inherit the
/// caller's session variables ([`crate::proc_tree::SESSION_VARS`]): it is
/// not the caller, and a tmux server it starts must not carry them either.
pub async fn launch_worker(workspace_root: &Path, pool: &SqlitePool, task_id: &str) -> Result<u32> {
    use std::io::Write;
    use std::os::unix::process::CommandExt;
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let armed = sqlx::query("UPDATE tasks SET run_token_sha256 = ?2 WHERE id = ?1 AND status = 'queued'")
        .bind(task_id)
        .bind(crate::caller::scope_token_hash(&token))
        .execute(pool)
        .await?
        .rows_affected();
    if armed != 1 {
        bail!("task {task_id} is not queued");
    }
    let exe = std::env::current_exe().context("locating the nucleus binary")?;
    let log_dir = workspace_root.join("memory/logs/tasks");
    std::fs::create_dir_all(&log_dir)?;
    let log_path = log_dir.join(format!("{task_id}.log"));
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["tasks", "run", task_id, "--workspace-root"])
        .arg(workspace_root)
        .current_dir(workspace_root)
        .stdin(std::process::Stdio::piped())
        .stdout(log.try_clone()?)
        .stderr(log);
    for var in crate::proc_tree::SESSION_VARS {
        cmd.env_remove(var);
    }
    // SAFETY: setsid is async-signal-safe and touches no Rust state.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("spawning the task worker")?;
    {
        let mut stdin = child.stdin.take().context("the worker's stdin")?;
        stdin.write_all(format!("{token}\n").as_bytes()).context("passing the run token")?;
    }
    let pid = child.id();
    sqlx::query("UPDATE tasks SET runner_pid = ?2, heartbeat_at = ?3 WHERE id = ?1 AND status = 'queued'")
        .bind(task_id)
        .bind(pid as i64)
        .bind(crate::timestamp::now())
        .execute(pool)
        .await?;
    Ok(pid)
}

/// Take the worker role for `task_id` with the run token [`launch_worker`]
/// passed. The token is cleared in the same statement, so it works once.
pub async fn consume_run_token(pool: &SqlitePool, task_id: &str, token: &str) -> Result<()> {
    let token = token.trim();
    if token.is_empty() {
        bail!(
            "`tasks run` is internal: only the worker `tasks start` launches may run a task (no \
             run token on stdin)"
        );
    }
    let took = sqlx::query(
        "UPDATE tasks SET run_token_sha256 = NULL
          WHERE id = ?1 AND status = 'queued' AND run_token_sha256 = ?2",
    )
    .bind(task_id)
    .bind(crate::caller::scope_token_hash(token))
    .execute(pool)
    .await?
    .rows_affected();
    if took != 1 {
        bail!(
            "`tasks run` refused: the run token does not match task {task_id} (it was used \
             already, or this process was not launched by `tasks start`)"
        );
    }
    Ok(())
}

/// The worker body behind `nucleus tasks run <id>` (after the run token is
/// consumed). Waits for a free slot,
/// runs the session to completion, records the outcome, delivers it.
pub async fn run_worker(
    settings: &Settings,
    workspace_root: &Path,
    task_id: &str,
    run_token: &str,
) -> Result<()> {
    let pool = open(workspace_root).await?;
    let task = get(&pool, task_id, &Scope::Operator).await?;
    consume_run_token(&pool, &task.id, run_token).await?;
    let pid = std::process::id();
    let cap = settings.tasks.max_concurrent.max(1) as i64;

    // 1. Wait for a slot. While waiting, reap tasks whose worker died, so a
    //    crashed worker does not hold a slot until someone runs a command.
    let mut logged_wait = false;
    loop {
        match claim(&pool, &task.id, pid, cap).await? {
            Claim::Claimed => break,
            Claim::NotQueued(s) => {
                tracing::info!(task = task.short_id(), status = s, "task no longer queued — worker exits");
                return Ok(());
            }
            Claim::NoSlot(running) => {
                if !logged_wait {
                    tracing::warn!(
                        task = task.short_id(),
                        running,
                        cap,
                        "tasks: concurrent-worker cap reached — task waits for a slot"
                    );
                    add_event(
                        &pool,
                        &task.id,
                        "queued_for_slot",
                        &format!("{running} workers running (limit {cap}); waiting for a slot"),
                    )
                    .await?;
                    logged_wait = true;
                }
                heartbeat(&pool, &task.id, pid).await?;
                if let Err(e) = sweep(workspace_root, &pool, &settings.tasks).await {
                    tracing::warn!(err = %e, "tasks: sweep while waiting for a slot failed");
                }
                tokio::time::sleep(CAP_POLL).await;
            }
        }
    }
    // The runtime limit counts from the claim, so a slow spawn or a long
    // brief is inside it.
    let mut sup = Supervisor::new(&settings.tasks, &task.id, pid);
    add_event(&pool, &task.id, "started", &format!("worker pid {pid}")).await?;

    // 2. Run the session; any error becomes a failed task.
    let outcome = drive_session(settings, workspace_root, &pool, &task, &mut sup).await;
    let (status, result, error) = match outcome {
        Ok(Outcome::Superseded) => {
            tracing::info!(task = task.short_id(), "task finished by another process — worker exits");
            return Ok(());
        }
        Ok(Outcome::Done(text)) => (TaskStatus::Done, Some(text), None),
        Ok(Outcome::Failed(e)) => (TaskStatus::Failed, None, Some(e)),
        Err(e) => (TaskStatus::Failed, None, Some(format!("{e:#}"))),
    };
    if finish(&pool, &task.id, status, result.as_deref(), error.as_deref()).await? {
        let t = get(&pool, &task.id, &Scope::Operator).await?;
        deliver(workspace_root, &pool, &settings.tasks, &t).await;
    }
    Ok(())
}

enum Outcome {
    Done(String),
    Failed(String),
    /// Another process finished the task (cancel, sweep): stop, write nothing.
    Superseded,
}

/// Cancel, runtime and heartbeat checks, run every second from the claim on
/// — during the session spawn, while the brief is typed, and while the
/// transcript is followed.
struct Supervisor {
    task_id: String,
    pid: u32,
    started: Instant,
    limit: Duration,
    limit_hours: u32,
    last_beat: Instant,
}

impl Supervisor {
    fn new(cfg: &TasksConfig, task_id: &str, pid: u32) -> Self {
        Self {
            task_id: task_id.to_string(),
            pid,
            started: Instant::now(),
            limit: Duration::from_secs(cfg.max_runtime_hours.max(1) as u64 * 3600),
            limit_hours: cfg.max_runtime_hours.max(1),
            last_beat: Instant::now(),
        }
    }

    /// `Some(outcome)` when the worker must stop.
    async fn check(&mut self, pool: &SqlitePool) -> Result<Option<Outcome>> {
        let status: String = sqlx::query_scalar("SELECT status FROM tasks WHERE id = ?1")
            .bind(&self.task_id)
            .fetch_one(pool)
            .await?;
        if status != "running" {
            return Ok(Some(Outcome::Superseded));
        }
        if self.started.elapsed() > self.limit {
            tracing::warn!(task = %self.task_id, "tasks: runtime guard hit — stopping the worker");
            add_event(
                pool,
                &self.task_id,
                "runtime_guard",
                &format!("stopped after {} h (tasks.max_runtime_hours)", self.limit_hours),
            )
            .await?;
            return Ok(Some(Outcome::Failed(format!(
                "stopped by the {} h runtime limit",
                self.limit_hours
            ))));
        }
        if self.last_beat.elapsed() >= HEARTBEAT_EVERY {
            self.last_beat = Instant::now();
            heartbeat(pool, &self.task_id, self.pid).await?;
        }
        Ok(None)
    }
}

async fn drive_session(
    settings: &Settings,
    workspace_root: &Path,
    pool: &SqlitePool,
    task: &Task,
    sup: &mut Supervisor,
) -> Result<Outcome> {
    let ctx = ProfileContext {
        workspace_root,
        claude: &settings.claude,
        tmux_session: &settings.tasks.tmux_session,
        agent_label: TASKS_AGENT_LABEL,
    };
    let profile = SessionProfile::one_shot_agentic(&ctx)
        .system_prompt(WORKER_PROMPT)
        .window_name(format!("task-{}", task.short_id()))
        .env(crate::proc_tree::ENV_SESSION, crate::proc_tree::SESSION_WORKER)
        .env(crate::caller::ENV_TASK_WORKER, task.id.clone())
        .extend_disallowed_tools(worker_denylist());
    let brief = worker_message(task);

    // Spawn and type the brief under supervision: a cancel, the runtime
    // limit or a lost row stops the setup too, and the heartbeat continues.
    let setup = async {
        let (mut session, _ask) = profile.spawn().await?;
        let recorded = sqlx::query(
            "UPDATE tasks SET session_id = ?2, tmux_window = ?3, transcript_path = ?4 WHERE id = ?1",
        )
        .bind(&task.id)
        .bind(session.session_id())
        .bind(session.tmux_target())
        .bind(session.transcript_path().to_string_lossy().to_string())
        .execute(pool)
        .await;
        if let Err(e) = recorded {
            let _ = session.close().await;
            return Err(anyhow::Error::from(e));
        }
        match session.submit_typed(&brief).await {
            Ok(offset) => Ok((session, offset)),
            Err(e) => {
                let _ = session.close().await;
                Err(e)
            }
        }
    };
    tokio::pin!(setup);
    let (mut session, offset) = loop {
        tokio::select! {
            r = &mut setup => break r?,
            _ = tokio::time::sleep(Duration::from_secs(1)) => {
                if let Some(o) = sup.check(pool).await? {
                    // Dropping the setup future abandons the half-built
                    // session; its window is found by name and killed.
                    let t = get(pool, &task.id, &Scope::Operator).await?;
                    kill_task_windows(&settings.tasks.tmux_session, &t).await;
                    return Ok(o);
                }
            }
        }
    };

    let result = follow(pool, task, sup, &mut session, offset).await;
    let _ = session.close().await;
    result
}

async fn follow(
    pool: &SqlitePool,
    task: &Task,
    sup: &mut Supervisor,
    session: &mut Session,
    mut offset: u64,
) -> Result<Outcome> {
    let transcript: PathBuf = session.transcript_path().to_path_buf();
    let identity = file_identity(&transcript).await;
    let mut tracker = TurnTracker::new();
    let mut last_alive_check = Instant::now();
    loop {
        // Read what the session appended.
        match read_from(&transcript, offset, identity).await {
            Read::Data(chunk, end) => {
                offset = end;
                for ev in tracker.feed(&chunk) {
                    if let Some(o) = on_event(pool, task, ev).await? {
                        return Ok(o);
                    }
                }
            }
            Read::Nothing => {}
            Read::Replaced => {
                return Ok(Outcome::Failed(
                    "the session transcript was truncated or replaced while the task ran; the \
                     worker cannot follow it"
                        .into(),
                ));
            }
        }

        if let Some(o) = sup.check(pool).await? {
            return Ok(o);
        }
        if last_alive_check.elapsed() >= Duration::from_secs(10) {
            last_alive_check = Instant::now();
            if !session.is_alive().await {
                return Ok(Outcome::Failed("the worker session window closed".into()));
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn on_event(pool: &SqlitePool, task: &Task, ev: TrackEvent) -> Result<Option<Outcome>> {
    match ev {
        TrackEvent::Progress { text } => {
            add_event(pool, &task.id, "progress", &clip(&text, 2_000)).await?;
        }
        TrackEvent::BgStarted { id } => {
            add_event(pool, &task.id, "background", &format!("background command {id} started")).await?;
        }
        TrackEvent::TurnEnd { final_text, pending_bg } if pending_bg > 0 => {
            // The model ended its turn while background work runs; the next
            // turn continues the task.
            let note = final_text.unwrap_or_default();
            let msg = format!("waiting for {pending_bg} background command(s). {}", clip(&note, 1_500));
            add_event(pool, &task.id, "progress", &msg).await?;
        }
        TrackEvent::TurnEnd { final_text, .. } => {
            let Some(text) = final_text else {
                return Ok(Some(Outcome::Failed(
                    "the session ended its turn without a final message".into(),
                )));
            };
            if let Some(what) = infra_failure_description(&text) {
                return Ok(Some(Outcome::Failed(format!("{what}: {}", clip(&text, 300)))));
            }
            return Ok(Some(Outcome::Done(text)));
        }
        _ => {}
    }
    Ok(None)
}

enum Read {
    Data(String, u64),
    Nothing,
    /// The file is shorter than what was already read, or is a different
    /// file than the one the worker started following.
    Replaced,
}

async fn file_identity(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    tokio::fs::metadata(path).await.ok().map(|m| m.ino())
}

/// Bytes appended to `path` past `offset`, cut at the last complete line so a
/// multi-byte character is never split across reads.
async fn read_from(path: &Path, offset: u64, identity: Option<u64>) -> Read {
    use std::os::unix::fs::MetadataExt;
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let Ok(meta) = tokio::fs::metadata(path).await else { return Read::Nothing };
    let len = meta.len();
    if len < offset || identity.map(|i| i != meta.ino()).unwrap_or(false) {
        return Read::Replaced;
    }
    if len == offset {
        return Read::Nothing;
    }
    let Ok(mut f) = tokio::fs::File::open(path).await else { return Read::Nothing };
    if f.seek(std::io::SeekFrom::Start(offset)).await.is_err() {
        return Read::Nothing;
    }
    let mut buf = Vec::with_capacity((len - offset) as usize);
    if f.take(len - offset).read_to_end(&mut buf).await.is_err() {
        return Read::Nothing;
    }
    let Some(cut) = buf.iter().rposition(|b| *b == b'\n').map(|i| i + 1) else {
        return Read::Nothing;
    };
    buf.truncate(cut);
    Read::Data(String::from_utf8_lossy(&buf).into_owned(), offset + cut as u64)
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

// ── delivery ─────────────────────────────────────────────────────────────

/// The operator-facing status line and body for a finished task.
pub fn outcome_message(t: &Task, texts: &crate::config::TaskTexts) -> String {
    let template = match t.status() {
        TaskStatus::Done => &texts.done,
        TaskStatus::Failed => &texts.failed,
        TaskStatus::Cancelled => &texts.cancelled,
        TaskStatus::Interrupted => &texts.interrupted,
        TaskStatus::Queued | TaskStatus::Running => &texts.failed,
    };
    let head = template.replace("{id}", t.short_id()).replace("{title}", &t.title);
    match (t.status(), &t.result, &t.error) {
        (TaskStatus::Done, Some(r), _) => format!("{head}\n\n{}", clip(r, DELIVERY_MAX_CHARS)),
        (TaskStatus::Failed | TaskStatus::Interrupted, _, Some(e)) => format!("{head}\n{}", clip(e, 1_000)),
        _ => head,
    }
}

/// The body of the context message for the originating chat session. The bot
/// types it inside the agent-message envelope (sender `task:<id>`, marked as
/// not written by the operator, every line prefixed), so the conversation can
/// continue about the result.
fn injection_body(t: &Task) -> String {
    let body = match (t.status(), &t.result, &t.error) {
        (TaskStatus::Done, Some(r), _) => format!("Result:\n{}", clip(r, INJECT_MAX_CHARS)),
        (_, _, Some(e)) => format!("Reason: {}", clip(e, 1_000)),
        _ => String::new(),
    };
    format!(
        "Background task {} ({:?}) ended with status {}. The operator already received this in \
         the chat; do not repeat it.\n{body}",
        t.short_id(),
        t.title,
        t.status
    )
}

/// What one delivery attempt achieved.
enum Delivered {
    /// The destination has the message (Discord).
    Sent(String),
    /// The message is queued for the WhatsApp bot (outbound row id); it is
    /// delivered when the bot marks that row sent ([`confirm_whatsapp`]).
    Queued { outbound_id: i64, note: String },
}

/// Deliver a finished task to its origin, at most once at a time. Never fails
/// the caller: the ledger already holds the outcome, every attempt is
/// recorded as an event, and [`sweep`] retries a delivery that did not
/// complete.
///
/// `delivered_at` means the destination has the message: for Discord, the
/// API returned the message; for WhatsApp, the bot marked the queued
/// outbound row sent.
///
/// A new message is sent only when the earlier attempt provably never left
/// this machine: a WhatsApp row the bot's drain refused before its first send
/// attempt ([`crate::whatsapp_queue::OutboundState::failed_untransmitted`]),
/// or a Discord request that got no connection or a 4xx answer
/// ([`crate::discord_sdk::NotSent`]). Such a failure is recorded
/// (`delivery_failed`) and retried until [`DELIVERY_MAX_FAILURES`]. A failure
/// after the message may have reached the destination (a timeout, a closed
/// connection, a 5xx answer) is never retried with a new message: the
/// delivery is given up (`delivery_failed_at`), and the operator gets one
/// note ([`note_given_up_deliveries`]).
pub async fn deliver(workspace_root: &Path, pool: &SqlitePool, cfg: &TasksConfig, t: &Task) {
    if !matches!(t.origin.as_str(), "whatsapp-dm" | "discord-home") || !t.status().is_terminal() {
        return;
    }
    // Claim: one deliverer per task; a claim older than the TTL is abandoned.
    let claimed = sqlx::query(
        "UPDATE tasks SET delivery_claimed_at = ?2
          WHERE id = ?1 AND delivered_at IS NULL AND delivery_queued_at IS NULL
            AND delivery_failed_at IS NULL
            AND (delivery_claimed_at IS NULL OR delivery_claimed_at < ?3)",
    )
    .bind(&t.id)
    .bind(crate::timestamp::now())
    .bind(ago(DELIVERY_CLAIM_TTL))
    .execute(pool)
    .await
    .map(|r| r.rows_affected() == 1)
    .unwrap_or(false);
    if !claimed {
        return;
    }
    let res = deliver_inner(workspace_root, pool, cfg, t).await;
    let mut tx = match pool.begin_with("BEGIN IMMEDIATE").await {
        Ok(tx) => tx,
        Err(e) => {
            tracing::warn!(task = t.short_id(), err = %e, "tasks: recording the delivery failed");
            return;
        }
    };
    let recorded: Result<()> = async {
        match &res {
            Ok(Delivered::Sent(msg)) => {
                sqlx::query("UPDATE tasks SET delivered_at = ?2, delivery_claimed_at = NULL WHERE id = ?1")
                    .bind(&t.id)
                    .bind(crate::timestamp::now())
                    .execute(&mut *tx)
                    .await?;
                add_event_on(&mut tx, &t.id, "delivered", msg).await?;
            }
            Ok(Delivered::Queued { outbound_id, note }) => {
                sqlx::query(
                    "UPDATE tasks SET delivery_queued_at = ?2, delivery_ref = ?3, delivery_claimed_at = NULL
                      WHERE id = ?1",
                )
                .bind(&t.id)
                .bind(crate::timestamp::now())
                .bind(outbound_id)
                .execute(&mut *tx)
                .await?;
                add_event_on(&mut tx, &t.id, "delivery_queued", note).await?;
            }
            Err(e) => {
                tracing::warn!(task = t.short_id(), err = %format!("{e:#}"), "tasks: delivery failed");
                sqlx::query("UPDATE tasks SET delivery_claimed_at = NULL WHERE id = ?1")
                    .bind(&t.id)
                    .execute(&mut *tx)
                    .await?;
                add_event_on(&mut tx, &t.id, "delivery_failed", &format!("{e:#}")).await?;
                if is_ambiguous(&e) {
                    give_up_on(&mut tx, &t.id, &format!("{e:#}")).await?;
                } else {
                    give_up_if_exhausted(&mut tx, &t.id, &format!("{e:#}")).await?;
                }
            }
        }
        Ok(())
    }
    .await;
    match recorded {
        Ok(()) => {
            let _ = tx.commit().await;
        }
        Err(e) => tracing::warn!(task = t.short_id(), err = %e, "tasks: recording the delivery failed"),
    }
}

/// A send whose outcome is unknown: the message may have reached the
/// destination. Never answered with a new message.
#[derive(Debug)]
struct AmbiguousSend(String);

impl std::fmt::Display for AmbiguousSend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AmbiguousSend {}

fn is_ambiguous(e: &anyhow::Error) -> bool {
    e.downcast_ref::<AmbiguousSend>().is_some()
}

/// Give the delivery up: it is not attempted again, and
/// [`note_given_up_deliveries`] sends the operator one note.
async fn give_up_on(tx: &mut sqlx::SqliteConnection, id: &str, reason: &str) -> Result<()> {
    let n = sqlx::query(
        "UPDATE tasks SET delivery_failed_at = ?2, delivery_error = ?3, delivery_claimed_at = NULL
          WHERE id = ?1 AND delivered_at IS NULL AND delivery_failed_at IS NULL",
    )
    .bind(id)
    .bind(crate::timestamp::now())
    .bind(clip(reason, 1_000))
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n == 1 {
        add_event_on(tx, id, "delivery_given_up", reason).await?;
    }
    Ok(())
}

/// Give the delivery up once [`DELIVERY_MAX_FAILURES`] attempts failed.
async fn give_up_if_exhausted(tx: &mut sqlx::SqliteConnection, id: &str, reason: &str) -> Result<()> {
    let failures: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM task_events WHERE task_id = ?1 AND kind = 'delivery_failed'")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if failures >= DELIVERY_MAX_FAILURES {
        give_up_on(tx, id, &format!("{failures} delivery attempts failed; the last: {reason}")).await?;
    }
    Ok(())
}

/// Settle queued WhatsApp deliveries from the bot's outbound rows:
///
/// - a sent row sets `delivered_at` (also for a delivery given up earlier:
///   the bot marks a failed row sent when the server acknowledges it late);
/// - a row that failed without ever being handed to the socket is recorded
///   as `delivery_failed` and released, so [`retry_deliveries`] queues the
///   message again under a new key;
/// - a row that failed after a send attempt, or that disappeared, has an
///   unknown outcome: the delivery is given up, never sent again.
async fn confirm_whatsapp(workspace_root: &Path, pool: &SqlitePool) -> Result<()> {
    let waiting: Vec<(String, Option<i64>, Option<String>)> = sqlx::query_as(
        "SELECT id, delivery_ref, delivery_failed_at FROM tasks
          WHERE delivered_at IS NULL AND delivery_queued_at IS NOT NULL",
    )
    .fetch_all(pool)
    .await?;
    if waiting.is_empty() {
        return Ok(());
    }
    let wa_path = workspace_root.join(crate::whatsapp_queue::WHATSAPP_DB_PATH);
    if !wa_path.exists() {
        return Ok(());
    }
    let wa = crate::db::open_read_only(&wa_path).await?;
    for (id, outbound, given_up) in waiting {
        let row = match outbound {
            Some(o) => crate::whatsapp_queue::outbound_status(&wa, o).await?,
            None => None,
        };
        let outbound = outbound.unwrap_or_default();
        let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
        match row {
            Some(r) if r.status == "sent" => {
                let n = sqlx::query(
                    "UPDATE tasks SET delivered_at = ?2
                      WHERE id = ?1 AND delivered_at IS NULL AND delivery_queued_at IS NOT NULL",
                )
                .bind(&id)
                .bind(r.sent_at.unwrap_or_else(crate::timestamp::now))
                .execute(&mut *tx)
                .await?
                .rows_affected();
                if n == 1 {
                    add_event_on(&mut tx, &id, "delivered", &format!("whatsapp outbound #{outbound} sent")).await?;
                }
            }
            // Given up already: only a late `sent` changes anything.
            _ if given_up.is_some() => {}
            Some(r) if r.status != "failed" => {}
            Some(r) if r.failed_untransmitted() => {
                let reason = format!(
                    "whatsapp outbound #{outbound} failed before any send attempt: {}",
                    r.last_error.unwrap_or_else(|| "no error recorded".into())
                );
                let n = sqlx::query(
                    "UPDATE tasks SET delivery_queued_at = NULL, delivery_ref = NULL,
                                      delivery_redrives = delivery_redrives + 1
                      WHERE id = ?1 AND delivered_at IS NULL AND delivery_queued_at IS NOT NULL",
                )
                .bind(&id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                if n == 1 {
                    add_event_on(&mut tx, &id, "delivery_failed", &reason).await?;
                    give_up_if_exhausted(&mut tx, &id, &reason).await?;
                }
            }
            other => {
                let reason = match other {
                    Some(r) => format!(
                        "whatsapp outbound #{outbound} failed after a send attempt, so the message may have \
                         arrived: {}",
                        r.last_error.unwrap_or_else(|| "no error recorded".into())
                    ),
                    None => format!("whatsapp outbound #{outbound} is gone from the queue; its outcome is unknown"),
                };
                add_event_on(&mut tx, &id, "delivery_failed", &reason).await?;
                give_up_on(&mut tx, &id, &reason).await?;
            }
        }
        tx.commit().await?;
    }
    wa.close().await;
    Ok(())
}

/// Send the operator one note for every delivery given up and not
/// delivered: the result may be missing, and it is not sent again. The note
/// goes to the task's origin under its own key, so a repeated sweep never
/// queues a second one; a Discord note that provably did not leave is tried
/// again on the next sweep.
async fn note_given_up_deliveries(workspace_root: &Path, pool: &SqlitePool, cfg: &TasksConfig) -> Result<()> {
    let tasks: Vec<Task> = sqlx::query_as(&format!(
        "SELECT {TASK_COLUMNS} FROM tasks
          WHERE delivery_failed_at IS NOT NULL AND delivery_noted_at IS NULL AND delivered_at IS NULL"
    ))
    .fetch_all(pool)
    .await?;
    for t in tasks {
        let body = cfg
            .texts
            .delivery_failed
            .replace("{id}", t.short_id())
            .replace("{title}", &t.title)
            .replace("{reason}", t.delivery_error.as_deref().unwrap_or("no reason recorded"));
        let key = format!("task:{}:{}:delivery-given-up", t.id, t.status);
        let sender = format!("task:{}", t.short_id());
        let noted: Result<bool> = async {
            match t.origin.as_str() {
                "whatsapp-dm" => {
                    let chat = match &t.origin_ref {
                        Some(r) => crate::whatsapp_queue::canonical_dm_chat(r)?,
                        None => crate::whatsapp_queue::INBOX_CHAT_OPERATOR_DM.to_string(),
                    };
                    let wa = crate::whatsapp_queue::open(workspace_root).await?;
                    let r = crate::whatsapp_queue::enqueue_text_once(&wa, &chat, &body, &sender, &key).await;
                    wa.close().await;
                    r?;
                    Ok(true)
                }
                "discord-home" => {
                    let channel = std::env::var("DISCORD_HOME_CHANNEL_ID")
                        .ok()
                        .filter(|c| !c.trim().is_empty())
                        .context("DISCORD_HOME_CHANNEL_ID is not set")?;
                    let res = outbox_send(pool, &format!("{key}:discord"), &t.id, "discord-home", &body, |b, nonce| {
                        async move { crate::discord_sdk::send_message_once(&channel, &b, true, &nonce).await }
                    })
                    .await;
                    match res {
                        Ok(_) => Ok(true),
                        // Unknown outcome: never a second note.
                        Err(e) if is_ambiguous(&e) => Ok(true),
                        Err(e) => Err(e),
                    }
                }
                _ => Ok(true),
            }
        }
        .await;
        match noted {
            Ok(true) => {
                let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
                let n = sqlx::query("UPDATE tasks SET delivery_noted_at = ?2 WHERE id = ?1 AND delivery_noted_at IS NULL")
                    .bind(&t.id)
                    .bind(crate::timestamp::now())
                    .execute(&mut *tx)
                    .await?
                    .rows_affected();
                if n == 1 {
                    add_event_on(&mut tx, &t.id, "delivery_noted", &format!("the operator was told ({})", t.origin))
                        .await?;
                }
                tx.commit().await?;
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(task = t.short_id(), err = %format!("{e:#}"), "tasks: the given-up note was not sent"),
        }
    }
    Ok(())
}

/// Send `body` once through the Discord outbox. The outbox row (unique
/// `key`) is written before the send and marked `sent` after it, so a retry
/// never posts a message the outbox records as sent. A failed send is tried
/// again only when the failure proves nothing was posted
/// ([`crate::discord_sdk::NotSent`]); any other failure (a timeout, a closed
/// connection, a 5xx answer) marks the row `unknown` and returns
/// [`AmbiguousSend`], and the row is never sent again. A retry after a crash
/// between the send and the mark sends again with the same Discord nonce,
/// which Discord deduplicates for a few minutes.
async fn outbox_send<F, Fut>(
    pool: &SqlitePool,
    key: &str,
    task_id: &str,
    channel: &str,
    body: &str,
    send: F,
) -> Result<String>
where
    F: FnOnce(String, String) -> Fut,
    Fut: std::future::Future<Output = Result<String>>,
{
    sqlx::query(
        "INSERT OR IGNORE INTO task_outbox (key, task_id, channel, body, status, created_at)
         VALUES (?1, ?2, ?3, ?4, 'pending', ?5)",
    )
    .bind(key)
    .bind(task_id)
    .bind(channel)
    .bind(body)
    .bind(crate::timestamp::now())
    .execute(pool)
    .await?;
    let (status, message_id, stored, last_error): (String, Option<String>, String, Option<String>) =
        sqlx::query_as("SELECT status, message_id, body, last_error FROM task_outbox WHERE key = ?1")
            .bind(key)
            .fetch_one(pool)
            .await?;
    match status.as_str() {
        "sent" => return Ok(message_id.unwrap_or_default()),
        "unknown" => {
            return Err(AmbiguousSend(format!(
                "discord outbox {key}: an earlier send may have posted the message ({}); not sent again",
                last_error.unwrap_or_default()
            ))
            .into())
        }
        _ => {}
    }
    sqlx::query("UPDATE task_outbox SET attempts = attempts + 1 WHERE key = ?1")
        .bind(key)
        .execute(pool)
        .await?;
    let nonce: String = crate::caller::scope_token_hash(key).chars().take(25).collect();
    match send(stored, nonce).await {
        Ok(id) => {
            sqlx::query(
                "UPDATE task_outbox SET status = 'sent', message_id = ?2, sent_at = ?3, last_error = NULL
                  WHERE key = ?1",
            )
            .bind(key)
            .bind(&id)
            .bind(crate::timestamp::now())
            .execute(pool)
            .await?;
            Ok(id)
        }
        Err(e) if crate::discord_sdk::not_sent(&e) => {
            sqlx::query("UPDATE task_outbox SET last_error = ?2 WHERE key = ?1")
                .bind(key)
                .bind(format!("{e:#}"))
                .execute(pool)
                .await?;
            Err(e)
        }
        Err(e) => {
            sqlx::query("UPDATE task_outbox SET status = 'unknown', last_error = ?2 WHERE key = ?1")
                .bind(key)
                .bind(format!("{e:#}"))
                .execute(pool)
                .await?;
            Err(e.context(AmbiguousSend(format!("discord outbox {key}: the send failed after it may have posted"))))
        }
    }
}

async fn deliver_inner(
    workspace_root: &Path,
    pool: &SqlitePool,
    cfg: &TasksConfig,
    t: &Task,
) -> Result<Delivered> {
    let key = format!("task:{}:{}", t.id, t.status);
    let sender = format!("task:{}", t.short_id());
    match t.origin.as_str() {
        "whatsapp-dm" => {
            // The exact chat the task came from, re-validated against the DM
            // allowlist; without one (a task started from a shell), the bot's
            // operator-DM resolution, used for both rows.
            let chat = match &t.origin_ref {
                Some(r) => crate::whatsapp_queue::canonical_dm_chat(r)?,
                None => crate::whatsapp_queue::INBOX_CHAT_OPERATOR_DM.to_string(),
            };
            let redrives: i64 = sqlx::query_scalar("SELECT delivery_redrives FROM tasks WHERE id = ?1")
                .bind(&t.id)
                .fetch_one(pool)
                .await?;
            let wa = crate::whatsapp_queue::open(workspace_root).await?;
            let body = injection_body(t);
            let (q, i) = crate::whatsapp_queue::enqueue_delivery(
                &wa,
                crate::whatsapp_queue::Delivery {
                    chat: &chat,
                    message: &outcome_message(t, &cfg.texts),
                    context: Some((&sender, &body)),
                    source: &sender,
                    dedup_key: &key,
                    message_attempt: redrives.max(0) as u32,
                },
            )
            .await?;
            wa.close().await;
            Ok(Delivered::Queued {
                outbound_id: q,
                note: format!("whatsapp outbound #{q}, chat-session inbox #{}", i.unwrap_or_default()),
            })
        }
        "discord-home" => {
            let channel = std::env::var("DISCORD_HOME_CHANNEL_ID")
                .ok()
                .filter(|c| !c.trim().is_empty())
                .context("DISCORD_HOME_CHANNEL_ID is not set")?;
            let rules = crate::secret_filter::CredentialRules::from_workspace(workspace_root);
            let body = rules.redact(&clip(&outcome_message(t, &cfg.texts), DISCORD_MAX_CHARS)).text;
            let id = outbox_send(pool, &format!("{key}:discord"), &t.id, "discord-home", &body, |b, nonce| {
                let channel = channel.clone();
                async move { crate::discord_sdk::send_message_once(&channel, &b, true, &nonce).await }
            })
            .await?;
            Ok(Delivered::Sent(format!("discord message {id}")))
        }
        o => bail!("origin {o} has no delivery"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one DM allowlist value every test in this crate uses (tests share
    /// the process environment).
    pub(crate) const TEST_DM: &str = "5511999999999";

    async fn temp_pool() -> (tempfile::TempDir, SqlitePool) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        let pool = open(dir.path()).await.unwrap();
        (dir, pool)
    }

    fn cfg() -> TasksConfig {
        TasksConfig { tmux_session: "nucleus-test-tasks-unit".into(), ..TasksConfig::default() }
    }

    fn new_task(title: &str) -> NewTask {
        NewTask {
            kind: "general".into(),
            title: title.into(),
            brief: "do the thing".into(),
            origin: "cli".into(),
            origin_ref: None,
            parent_id: None,
            requested_by: "cli".into(),
            links: vec![("issue".into(), "example#1".into())],
        }
    }

    fn dm_task(title: &str, chat: &str) -> NewTask {
        std::env::set_var("WHATSAPP_ALLOWED_DM_JIDS", TEST_DM);
        let mut t = new_task(title);
        t.origin = "whatsapp-dm".into();
        t.origin_ref = Some(chat.into());
        t.requested_by = "model".into();
        t
    }

    #[tokio::test]
    async fn create_get_by_prefix_and_links() {
        let (_d, pool) = temp_pool().await;
        let t = create(&pool, new_task("a"), &Scope::Operator).await.unwrap();
        assert_eq!(t.status, "queued");
        let got = get(&pool, &t.id[..6], &Scope::Operator).await.unwrap();
        assert_eq!(got.id, t.id);
        assert!(get(&pool, "ab", &Scope::Operator).await.is_err(), "short prefixes are refused");
        assert!(get(&pool, "%%%%", &Scope::Operator).await.is_err(), "LIKE wildcards are refused");
        let l = links(&pool, &t.id).await.unwrap();
        assert_eq!(l.len(), 1);
        assert_eq!(events(&pool, &t.id).await.unwrap()[0].kind, "created");
    }

    #[tokio::test]
    async fn invalid_input_is_refused() {
        let (_d, pool) = temp_pool().await;
        let mut t = new_task("a");
        t.origin = "sms".into();
        assert!(create(&pool, t, &Scope::Operator).await.is_err());
        let mut t = new_task("a");
        t.brief = "  ".into();
        assert!(create(&pool, t, &Scope::Operator).await.is_err());
        let mut t = new_task("a");
        t.requested_by = "someone else".into();
        assert!(create(&pool, t, &Scope::Operator).await.is_err(), "requested_by is an enum");
        let mut t = new_task("a");
        t.brief = "x".repeat(MAX_BRIEF_CHARS + 1);
        let err = create(&pool, t, &Scope::Operator).await.unwrap_err();
        assert!(format!("{err:#}").contains("limit"), "{err:#}");
        // A DM origin_ref must be an allowlisted DM chat.
        // Synthetic ids built at runtime (the committed-secrets scanner reads
        // a literal `<digits>@<domain>` as a real JID).
        let not_allowed = format!("{}@s.whatsapp.net", "5511888888888");
        let group = format!("{}@g.us", "120363000000000000");
        assert!(create(&pool, dm_task("a", &not_allowed), &Scope::Operator).await.is_err());
        assert!(create(&pool, dm_task("a", &group), &Scope::Operator).await.is_err());
        let ok = create(&pool, dm_task("a", "+55 11 99999-9999"), &Scope::Operator).await.unwrap();
        assert_eq!(ok.origin_ref.as_deref(), Some("5511999999999@s.whatsapp.net"));
    }

    #[tokio::test]
    async fn scope_limits_what_a_chat_sees() {
        let (d, pool) = temp_pool().await;
        let mine = create(&pool, dm_task("mine", "5511999999999@s.whatsapp.net"), &Scope::Operator).await.unwrap();
        let other = create(&pool, new_task("cli task"), &Scope::Operator).await.unwrap();
        let scope = Scope::Origin {
            origin: "whatsapp-dm".into(),
            origin_ref: "5511999999999@s.whatsapp.net".into(),
        };
        let seen = list(&pool, false, 50, &scope).await.unwrap();
        assert_eq!(seen.iter().map(|t| t.id.clone()).collect::<Vec<_>>(), vec![mine.id.clone()]);
        assert!(get(&pool, &other.id, &scope).await.is_err(), "another origin's task does not exist for the chat");
        assert!(request_cancel(d.path(), &pool, &cfg(), &other.id, &scope).await.is_err());
        assert_eq!(get(&pool, &other.id, &Scope::Operator).await.unwrap().status, "queued");
        assert_eq!(list(&pool, false, 50, &Scope::Operator).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn claim_respects_the_concurrency_cap() {
        let (_d, pool) = temp_pool().await;
        let a = create(&pool, new_task("a"), &Scope::Operator).await.unwrap();
        let b = create(&pool, new_task("b"), &Scope::Operator).await.unwrap();
        assert!(matches!(claim(&pool, &a.id, 1, 1).await.unwrap(), Claim::Claimed));
        assert!(matches!(claim(&pool, &b.id, 2, 1).await.unwrap(), Claim::NoSlot(1)));
        assert!(matches!(claim(&pool, &a.id, 1, 1).await.unwrap(), Claim::NotQueued(_)));
        assert!(finish(&pool, &a.id, TaskStatus::Done, Some("ok"), None).await.unwrap());
        assert!(matches!(claim(&pool, &b.id, 2, 1).await.unwrap(), Claim::Claimed));
    }

    #[tokio::test]
    async fn cancel_is_an_immediate_terminal_transition() {
        let (d, pool) = temp_pool().await;
        let a = create(&pool, new_task("a"), &Scope::Operator).await.unwrap();
        let c = request_cancel(d.path(), &pool, &cfg(), &a.id, &Scope::Operator).await.unwrap();
        assert_eq!(c.status, "cancelled");
        assert!(request_cancel(d.path(), &pool, &cfg(), &a.id, &Scope::Operator).await.is_err());

        let b = create(&pool, new_task("b"), &Scope::Operator).await.unwrap();
        claim(&pool, &b.id, 1, 6).await.unwrap();
        let c = request_cancel(d.path(), &pool, &cfg(), &b.id, &Scope::Operator).await.unwrap();
        assert_eq!(c.status, "cancelled", "a running task is cancelled at once, not flagged");
        // The worker's supervisor then stops without writing.
        let mut sup = Supervisor::new(&cfg(), &b.id, 1);
        assert!(matches!(sup.check(&pool).await.unwrap(), Some(Outcome::Superseded)));
        // A late finish by the worker changes nothing.
        assert!(!finish(&pool, &b.id, TaskStatus::Done, Some("late"), None).await.unwrap());
        assert_eq!(get(&pool, &b.id, &Scope::Operator).await.unwrap().status, "cancelled");
    }

    #[tokio::test]
    async fn sweep_marks_tasks_without_a_fresh_heartbeat_or_a_live_worker() {
        let (d, pool) = temp_pool().await;
        let a = create(&pool, new_task("stale"), &Scope::Operator).await.unwrap();
        let b = create(&pool, new_task("fresh"), &Scope::Operator).await.unwrap();
        let c = create(&pool, new_task("stale heartbeat, live worker"), &Scope::Operator).await.unwrap();
        claim(&pool, &a.id, 1, 6).await.unwrap();
        claim(&pool, &b.id, 2, 6).await.unwrap();
        claim(&pool, &c.id, 3, 6).await.unwrap();
        for id in [&a.id, &c.id] {
            sqlx::query("UPDATE tasks SET heartbeat_at = '2020-01-01T00:00:00.000Z', runner_pid = NULL WHERE id = ?1")
                .bind(id)
                .execute(&pool)
                .await
                .unwrap();
        }
        // c's worker is "alive": a real process whose command line names the
        // task, like `nucleus tasks run <id>`.
        let mut child = std::process::Command::new("sh")
            .args(["-c", "sleep 30; true", "tasks run", &c.id])
            .spawn()
            .unwrap();
        sqlx::query("UPDATE tasks SET runner_pid = ?2 WHERE id = ?1")
            .bind(&c.id)
            .bind(child.id() as i64)
            .execute(&pool)
            .await
            .unwrap();
        let marked = sweep(d.path(), &pool, &cfg()).await.unwrap();
        let _ = child.kill();
        assert_eq!(marked.iter().map(|t| t.id.clone()).collect::<Vec<_>>(), vec![a.id.clone()]);
        assert_eq!(get(&pool, &a.id, &Scope::Operator).await.unwrap().status, "interrupted");
        assert_eq!(get(&pool, &b.id, &Scope::Operator).await.unwrap().status, "running");
        assert_eq!(get(&pool, &c.id, &Scope::Operator).await.unwrap().status, "running");
    }

    #[tokio::test]
    async fn the_interrupt_rechecks_the_heartbeat_it_read() {
        let (_d, pool) = temp_pool().await;
        let a = create(&pool, new_task("a"), &Scope::Operator).await.unwrap();
        claim(&pool, &a.id, 1, 6).await.unwrap();
        let read = get(&pool, &a.id, &Scope::Operator).await.unwrap().heartbeat_at.unwrap();
        // The worker writes a heartbeat after the sweep read the row.
        tokio::time::sleep(Duration::from_millis(5)).await;
        heartbeat(&pool, &a.id, 1).await.unwrap();
        let done = finish_if(&pool, &a.id, TaskStatus::Interrupted, None, Some("x"), Some(&read))
            .await
            .unwrap();
        assert!(!done, "a fresh heartbeat wins over the stale read");
    }

    /// Mark the task's queued outbound row with `status`, as the bot's drain
    /// does.
    async fn set_outbound(d: &Path, pool: &SqlitePool, id: &str, status: &str) {
        let outbound: i64 = sqlx::query_scalar("SELECT delivery_ref FROM tasks WHERE id = ?1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
        let wa = crate::whatsapp_queue::open(d).await.unwrap();
        sqlx::query("UPDATE outbound_queue SET status = ?2, sent_at = ?3, last_error = ?4 WHERE id = ?1")
            .bind(outbound)
            .bind(status)
            .bind((status == "sent").then(crate::timestamp::now))
            .bind((status == "failed").then_some("unknown target: 5511999999999@s.whatsapp.net"))
            .execute(&wa)
            .await
            .unwrap();
        wa.close().await;
    }

    /// The bot's drain started a send of the task's row (fixed its message id,
    /// as `markInFlight` does), then saw `err`; `status` is where the row
    /// ended.
    async fn attempt_outbound(d: &Path, pool: &SqlitePool, id: &str, status: &str, err: &str) {
        let outbound: i64 = sqlx::query_scalar("SELECT delivery_ref FROM tasks WHERE id = ?1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
        let wa = crate::whatsapp_queue::open(d).await.unwrap();
        sqlx::query(
            "UPDATE outbound_queue SET status = ?2, msg_id = COALESCE(msg_id, '3EB0TESTMSGID'), attempts = attempts + 1,
                                       last_error = ?3 WHERE id = ?1",
        )
        .bind(outbound)
        .bind(status)
        .bind(err)
        .execute(&wa)
        .await
        .unwrap();
        wa.close().await;
    }

    async fn outbound_rows(d: &Path) -> Vec<(String, String)> {
        let wa = crate::whatsapp_queue::open(d).await.unwrap();
        let rows = sqlx::query_as("SELECT dedup_key, body FROM outbound_queue ORDER BY id").fetch_all(&wa).await.unwrap();
        wa.close().await;
        rows
    }

    #[tokio::test]
    async fn whatsapp_delivery_goes_to_the_origin_chat_once() {
        let (d, pool) = temp_pool().await;
        let t = create(&pool, dm_task("report", "5511999999999@s.whatsapp.net"), &Scope::Operator).await.unwrap();
        claim(&pool, &t.id, 1, 6).await.unwrap();
        finish(&pool, &t.id, TaskStatus::Done, Some("42 lines"), None).await.unwrap();
        let t = get(&pool, &t.id, &Scope::Operator).await.unwrap();
        deliver(d.path(), &pool, &cfg(), &t).await;
        let t = get(&pool, &t.id, &Scope::Operator).await.unwrap();
        assert!(t.delivery_queued_at.is_some(), "{:?}", events(&pool, &t.id).await.unwrap());
        // A second delivery (a retry that raced) adds nothing.
        deliver(d.path(), &pool, &cfg(), &t).await;
        sweep(d.path(), &pool, &cfg()).await.unwrap();
        let wa = crate::whatsapp_queue::open(d.path()).await.unwrap();
        let rows: Vec<(String, String)> = sqlx::query_as("SELECT target, body FROM outbound_queue")
            .fetch_all(&wa)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "5511999999999@s.whatsapp.net", "the visible message goes to the origin chat");
        assert!(rows[0].1.contains("done") && rows[0].1.contains("42 lines"), "{}", rows[0].1);
        let inbox: Vec<(String, String, String)> =
            sqlx::query_as("SELECT payload, chat, sender FROM session_inbox").fetch_all(&wa).await.unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].1, "5511999999999@s.whatsapp.net", "the context goes to the same chat");
        assert!(inbox[0].2.starts_with("task:"));
        assert!(!inbox[0].0.contains("[agent-msg"), "the bot builds the envelope, not the producer");
    }

    #[tokio::test]
    async fn sweep_retries_an_unfinished_delivery() {
        let (d, pool) = temp_pool().await;
        let t = create(&pool, dm_task("report", "5511999999999@s.whatsapp.net"), &Scope::Operator).await.unwrap();
        claim(&pool, &t.id, 1, 6).await.unwrap();
        // The worker finished and died before delivering.
        finish(&pool, &t.id, TaskStatus::Done, Some("r"), None).await.unwrap();
        sweep(d.path(), &pool, &cfg()).await.unwrap();
        assert!(get(&pool, &t.id, &Scope::Operator).await.unwrap().delivery_queued_at.is_some());
        // An abandoned claim (the deliverer died mid-delivery) is taken again
        // once it is old enough.
        let u = create(&pool, dm_task("r2", "5511999999999@s.whatsapp.net"), &Scope::Operator).await.unwrap();
        claim(&pool, &u.id, 1, 6).await.unwrap();
        finish(&pool, &u.id, TaskStatus::Done, Some("r"), None).await.unwrap();
        sqlx::query("UPDATE tasks SET delivery_claimed_at = ?2 WHERE id = ?1")
            .bind(&u.id)
            .bind(crate::timestamp::now())
            .execute(&pool)
            .await
            .unwrap();
        sweep(d.path(), &pool, &cfg()).await.unwrap();
        assert!(get(&pool, &u.id, &Scope::Operator).await.unwrap().delivery_queued_at.is_none(), "a fresh claim is respected");
        sqlx::query("UPDATE tasks SET delivery_claimed_at = '2020-01-01T00:00:00.000Z' WHERE id = ?1")
            .bind(&u.id)
            .execute(&pool)
            .await
            .unwrap();
        sweep(d.path(), &pool, &cfg()).await.unwrap();
        assert!(get(&pool, &u.id, &Scope::Operator).await.unwrap().delivery_queued_at.is_some());
    }

    #[tokio::test]
    async fn whatsapp_delivery_is_confirmed_only_when_the_row_is_sent() {
        let (d, pool) = temp_pool().await;
        let t = create(&pool, dm_task("report", "5511999999999@s.whatsapp.net"), &Scope::Operator).await.unwrap();
        claim(&pool, &t.id, 1, 6).await.unwrap();
        finish(&pool, &t.id, TaskStatus::Done, Some("r"), None).await.unwrap();
        let t = get(&pool, &t.id, &Scope::Operator).await.unwrap();
        deliver(d.path(), &pool, &cfg(), &t).await;
        // Queued, not delivered: the bot has not sent the row.
        sweep(d.path(), &pool, &cfg()).await.unwrap();
        assert!(get(&pool, &t.id, &Scope::Operator).await.unwrap().delivered_at.is_none());
        // The row failed for good: the message is queued again under a new
        // key; the context is not repeated.
        set_outbound(d.path(), &pool, &t.id, "failed").await;
        sweep(d.path(), &pool, &cfg()).await.unwrap();
        let after = get(&pool, &t.id, &Scope::Operator).await.unwrap();
        assert!(after.delivered_at.is_none());
        assert!(after.delivery_queued_at.is_some(), "requeued in the same sweep");
        let wa = crate::whatsapp_queue::open(d.path()).await.unwrap();
        let keys: Vec<String> = sqlx::query_scalar("SELECT dedup_key FROM outbound_queue ORDER BY id")
            .fetch_all(&wa)
            .await
            .unwrap();
        assert_eq!(keys.len(), 2, "{keys:?}");
        assert!(keys[1].ends_with(":message:r1"), "{keys:?}");
        let inbox: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session_inbox").fetch_one(&wa).await.unwrap();
        assert_eq!(inbox, 1);
        wa.close().await;
        // The new row is sent: delivered.
        set_outbound(d.path(), &pool, &t.id, "sent").await;
        sweep(d.path(), &pool, &cfg()).await.unwrap();
        let done = get(&pool, &t.id, &Scope::Operator).await.unwrap();
        assert!(done.delivered_at.is_some(), "{:?}", events(&pool, &t.id).await.unwrap());
        let kinds: Vec<String> = events(&pool, &t.id).await.unwrap().into_iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&"delivery_failed".to_string()), "{kinds:?}");
        assert_eq!(kinds.last().map(String::as_str), Some("delivered"), "{kinds:?}");
    }

    /// The server accepted the message, and the client saw a failure (the
    /// connection closed after the message left): the result is never
    /// queued again, the delivery is given up, and the operator gets exactly
    /// one note. The server's late acknowledgement still marks it delivered.
    #[tokio::test]
    async fn an_ambiguous_whatsapp_failure_is_given_up_not_resent() {
        let (d, pool) = temp_pool().await;
        let t = create(&pool, dm_task("report", "5511999999999@s.whatsapp.net"), &Scope::Operator).await.unwrap();
        claim(&pool, &t.id, 1, 6).await.unwrap();
        finish(&pool, &t.id, TaskStatus::Done, Some("r"), None).await.unwrap();
        let t = get(&pool, &t.id, &Scope::Operator).await.unwrap();
        deliver(d.path(), &pool, &cfg(), &t).await;
        attempt_outbound(d.path(), &pool, &t.id, "in_flight", "sendMessage timed out after 20000ms").await;
        sweep(d.path(), &pool, &cfg()).await.unwrap();
        assert!(get(&pool, &t.id, &Scope::Operator).await.unwrap().delivery_failed_at.is_none(), "in flight: wait");
        attempt_outbound(d.path(), &pool, &t.id, "failed", "Connection Closed").await;
        for _ in 0..3 {
            sweep(d.path(), &pool, &cfg()).await.unwrap();
        }
        let after = get(&pool, &t.id, &Scope::Operator).await.unwrap();
        assert!(after.delivered_at.is_none());
        assert!(after.delivery_failed_at.is_some(), "{:?}", events(&pool, &t.id).await.unwrap());
        assert!(after.delivery_error.as_deref().unwrap_or("").contains("Connection Closed"), "{:?}", after.delivery_error);
        let rows = outbound_rows(d.path()).await;
        let messages: Vec<_> = rows.iter().filter(|(k, _)| k.contains(":message")).collect();
        assert_eq!(messages.len(), 1, "no second result message: {rows:?}");
        let notes: Vec<_> = rows.iter().filter(|(k, _)| k.ends_with(":delivery-given-up")).collect();
        assert_eq!(notes.len(), 1, "one note: {rows:?}");
        assert!(notes[0].1.contains(t.short_id()) && notes[0].1.contains("not sent again"), "{}", notes[0].1);
        assert_eq!(rows.len(), 2, "{rows:?}");
        // The server acknowledges the message late; the bot marks the row sent.
        set_outbound(d.path(), &pool, &t.id, "sent").await;
        sweep(d.path(), &pool, &cfg()).await.unwrap();
        assert!(get(&pool, &t.id, &Scope::Operator).await.unwrap().delivered_at.is_some());
    }

    /// A row that disappeared from the queue has an unknown outcome too.
    #[tokio::test]
    async fn a_vanished_whatsapp_row_is_given_up() {
        let (d, pool) = temp_pool().await;
        let t = create(&pool, dm_task("report", "5511999999999@s.whatsapp.net"), &Scope::Operator).await.unwrap();
        claim(&pool, &t.id, 1, 6).await.unwrap();
        finish(&pool, &t.id, TaskStatus::Done, Some("r"), None).await.unwrap();
        let t = get(&pool, &t.id, &Scope::Operator).await.unwrap();
        deliver(d.path(), &pool, &cfg(), &t).await;
        let wa = crate::whatsapp_queue::open(d.path()).await.unwrap();
        sqlx::query("DELETE FROM outbound_queue").execute(&wa).await.unwrap();
        wa.close().await;
        sweep(d.path(), &pool, &cfg()).await.unwrap();
        assert!(get(&pool, &t.id, &Scope::Operator).await.unwrap().delivery_failed_at.is_some());
        let rows = outbound_rows(d.path()).await;
        assert!(rows.iter().all(|(k, _)| !k.contains(":message")), "{rows:?}");
    }

    /// Failures that never reach the socket are retried with a new message
    /// until DELIVERY_MAX_FAILURES; then the delivery is given up, noted once.
    #[tokio::test]
    async fn untransmitted_failures_are_retried_then_given_up() {
        let (d, pool) = temp_pool().await;
        let t = create(&pool, dm_task("report", "5511999999999@s.whatsapp.net"), &Scope::Operator).await.unwrap();
        claim(&pool, &t.id, 1, 6).await.unwrap();
        finish(&pool, &t.id, TaskStatus::Done, Some("r"), None).await.unwrap();
        let t = get(&pool, &t.id, &Scope::Operator).await.unwrap();
        deliver(d.path(), &pool, &cfg(), &t).await;
        for _ in 0..DELIVERY_MAX_FAILURES {
            set_outbound(d.path(), &pool, &t.id, "failed").await;
            sweep(d.path(), &pool, &cfg()).await.unwrap();
        }
        let after = get(&pool, &t.id, &Scope::Operator).await.unwrap();
        assert!(after.delivery_failed_at.is_some(), "{:?}", events(&pool, &t.id).await.unwrap());
        let rows = outbound_rows(d.path()).await;
        let messages = rows.iter().filter(|(k, _)| k.contains(":message")).count();
        assert_eq!(messages as i64, DELIVERY_MAX_FAILURES, "{rows:?}");
        assert_eq!(rows.iter().filter(|(k, _)| k.ends_with(":delivery-given-up")).count(), 1, "{rows:?}");
    }

    #[tokio::test]
    async fn discord_outbox_does_not_resend_an_ambiguous_failure() {
        let (_d, pool) = temp_pool().await;
        let t = create(&pool, new_task("d"), &Scope::Operator).await.unwrap();
        let sends = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let send = |err: Option<bool>| {
            let sends = sends.clone();
            move |_body: String, _nonce: String| async move {
                sends.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                match err {
                    Some(true) => Err(crate::discord_sdk::NotSent("discord POST failed: 403".into()).into()),
                    Some(false) => anyhow::bail!("posting message to discord: operation timed out"),
                    None => Ok("m1".to_string()),
                }
            }
        };
        let key = format!("task:{}:done:discord", t.id);
        // A 4xx proves nothing was posted: tried again.
        let e = outbox_send(&pool, &key, &t.id, "discord-home", "body", send(Some(true))).await.unwrap_err();
        assert!(!is_ambiguous(&e));
        // A timeout may have posted: the row becomes unknown and is not sent again.
        let e = outbox_send(&pool, &key, &t.id, "discord-home", "body", send(Some(false))).await.unwrap_err();
        assert!(is_ambiguous(&e), "{e:#}");
        let e = outbox_send(&pool, &key, &t.id, "discord-home", "body", send(None)).await.unwrap_err();
        assert!(is_ambiguous(&e), "{e:#}");
        assert_eq!(sends.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn discord_outbox_sends_once() {
        let (_d, pool) = temp_pool().await;
        let t = create(&pool, new_task("d"), &Scope::Operator).await.unwrap();
        let sends = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let send = |fail: bool| {
            let sends = sends.clone();
            let pool = pool.clone();
            move |body: String, nonce: String| async move {
                // The outbox row exists before the message leaves.
                let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_outbox").fetch_one(&pool).await.unwrap();
                assert_eq!(n, 1);
                assert_eq!(nonce.len(), 25);
                assert_eq!(body, "body");
                sends.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if fail {
                    return Err(crate::discord_sdk::NotSent("discord POST failed: 429".into()).into());
                }
                Ok("m1".to_string())
            }
        };
        let key = format!("task:{}:done:discord", t.id);
        assert!(outbox_send(&pool, &key, &t.id, "discord-home", "body", send(true)).await.is_err());
        assert_eq!(outbox_send(&pool, &key, &t.id, "discord-home", "body", send(false)).await.unwrap(), "m1");
        // Once the outbox records it sent, a later retry does not post again.
        assert_eq!(outbox_send(&pool, &key, &t.id, "discord-home", "body", send(false)).await.unwrap(), "m1");
        assert_eq!(sends.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn parent_resolves_in_the_callers_scope() {
        let (_d, pool) = temp_pool().await;
        let other = create(&pool, new_task("operator task"), &Scope::Operator).await.unwrap();
        let chat = Scope::Origin { origin: "whatsapp-dm".into(), origin_ref: "5511999999999@s.whatsapp.net".into() };
        let mine = create(&pool, dm_task("mine", "5511999999999@s.whatsapp.net"), &chat).await.unwrap();
        let mut child = dm_task("child", "5511999999999@s.whatsapp.net");
        child.parent_id = Some(other.id.clone());
        let err = create(&pool, child.clone(), &chat).await.unwrap_err();
        assert!(format!("{err:#}").contains("no task with id"), "{err:#}");
        child.parent_id = Some(mine.id[..6].to_string());
        assert_eq!(create(&pool, child, &chat).await.unwrap().parent_id, Some(mine.id));
    }

    #[tokio::test]
    async fn the_run_token_opens_the_task_once() {
        let (_d, pool) = temp_pool().await;
        let t = create(&pool, new_task("t"), &Scope::Operator).await.unwrap();
        // Without a launch, no token matches.
        assert!(consume_run_token(&pool, &t.id, "").await.is_err());
        assert!(consume_run_token(&pool, &t.id, "guess").await.is_err());
        sqlx::query("UPDATE tasks SET run_token_sha256 = ?2 WHERE id = ?1")
            .bind(&t.id)
            .bind(crate::caller::scope_token_hash("tok"))
            .execute(&pool)
            .await
            .unwrap();
        assert!(consume_run_token(&pool, &t.id, "wrong").await.is_err());
        consume_run_token(&pool, &t.id, "tok\n").await.unwrap();
        assert!(consume_run_token(&pool, &t.id, "tok").await.is_err(), "a token works once");
    }

    #[tokio::test]
    async fn transcript_replacement_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jsonl");
        std::fs::write(&p, "{\"a\":1}\n{\"b\":2}\n").unwrap();
        let id = file_identity(&p).await;
        let Read::Data(_, end) = read_from(&p, 0, id).await else { panic!("data expected") };
        assert!(matches!(read_from(&p, end, id).await, Read::Nothing));
        std::fs::write(&p, "{\"a\":1}\n").unwrap(); // truncated
        assert!(matches!(read_from(&p, end, id).await, Read::Replaced));
        let q = dir.path().join("u.jsonl");
        std::fs::write(&q, "{\"a\":1}\n{\"b\":2}\n{\"c\":3}\n").unwrap();
        std::fs::rename(&q, &p).unwrap(); // replaced by another file
        assert!(matches!(read_from(&p, end, id).await, Read::Replaced));
    }

    #[test]
    fn outcome_message_shapes() {
        let mut t = Task {
            id: "0123456789abcdef".into(),
            kind: "general".into(),
            title: "T".into(),
            brief: "b".into(),
            origin: "cli".into(),
            origin_ref: None,
            parent_id: None,
            requested_by: "cli".into(),
            status: "failed".into(),
            created_at: "x".into(),
            started_at: None,
            finished_at: None,
            heartbeat_at: None,
            cancel_requested_at: None,
            runner_pid: None,
            session_id: None,
            tmux_window: None,
            transcript_path: None,
            result: None,
            error: Some("boom".into()),
            delivered_at: None,
            delivery_claimed_at: None,
            delivery_queued_at: None,
            delivery_failed_at: None,
            delivery_error: None,
        };
        let texts = crate::config::TaskTexts::default();
        assert_eq!(outcome_message(&t, &texts), "⚠️ Task 01234567 failed — T\nboom");
        t.status = "done".into();
        t.result = Some("r".into());
        assert_eq!(outcome_message(&t, &texts), "✅ Task 01234567 done — T\n\nr");
    }
}
