//! Rust-side writers for the WhatsApp bot's queue tables (ADR-020 / ADR-033).
//!
//! `memory/whatsapp.db` is owned by the TypeScript bot. The one sanctioned
//! cross-process write is a queue table the bot drains (ADR-020 §5). Two such
//! tables exist:
//!
//! - `outbound_queue` — a message the bot sends to a WhatsApp chat (reminders,
//!   task results).
//! - `session_inbox` — a message from another Nucleus process that the bot
//!   types into one of its own chat sessions as context (task results,
//!   `session-send --to whatsapp-dm`). The row carries the sender and the
//!   body; the bot builds the attribution envelope when it types the message.
//!   The bot's turn engine owns typing into its sessions; a second process
//!   typing into the same tmux pane would interleave keystrokes with the
//!   engine, so other processes queue here instead (ADR-033).
//!
//! `dedup_key` makes an insert idempotent: a producer that retries a delivery
//! (the task delivery sweeper) inserts with the same key, and the second
//! insert is ignored. Both rows of one task delivery are inserted in one
//! transaction.
//!
//! The `CREATE TABLE IF NOT EXISTS` statements and the column additions below
//! exist only so a fresh install works before the bot has booted once, and so
//! a producer never fails on a DB the bot created before a column existed.
//! They must match `messaging/whatsapp/src/db.ts`, which owns the schema; the
//! additions are additive and idempotent, like the bot's own.

use anyhow::{Context, Result};
use sqlx::SqlitePool;
use std::path::Path;

/// Relative to the workspace root.
pub const WHATSAPP_DB_PATH: &str = "memory/whatsapp.db";

/// `session_inbox.chat` / `outbound_queue.target` value that means "the
/// operator's DM chat". The bot resolves it, for both tables with the same
/// function, to the DM chat it last talked to (or the first
/// `WHATSAPP_ALLOWED_DM_JIDS` entry), so producers never handle a JID and the
/// visible message and the context land in the same chat.
pub const INBOX_CHAT_OPERATOR_DM: &str = "dm";

/// Open whatsapp.db for queue inserts.
pub async fn open(workspace_root: &Path) -> Result<SqlitePool> {
    let pool = crate::db::open(&workspace_root.join(WHATSAPP_DB_PATH)).await?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS outbound_queue (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            target       TEXT    NOT NULL,
            body         TEXT    NOT NULL,
            source       TEXT    NOT NULL,
            enqueued_at  TEXT    NOT NULL,
            status       TEXT    NOT NULL DEFAULT 'pending',
            attempts     INTEGER NOT NULL DEFAULT 0,
            last_error   TEXT,
            sent_at      TEXT,
            msg_id       TEXT,
            kind         TEXT    NOT NULL DEFAULT 'text',
            media_path   TEXT,
            mimetype     TEXT,
            filename     TEXT,
            quoted_json  TEXT,
            in_flight_at TEXT,
            dedup_key    TEXT
        )
        "#,
    )
    .execute(&pool)
    .await?;
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS session_inbox (
            id           INTEGER PRIMARY KEY AUTOINCREMENT,
            chat         TEXT    NOT NULL,
            sender       TEXT    NOT NULL,
            payload      TEXT    NOT NULL,
            source       TEXT    NOT NULL,
            enqueued_at  TEXT    NOT NULL,
            status       TEXT    NOT NULL DEFAULT 'pending',
            attempts     INTEGER NOT NULL DEFAULT 0,
            last_error   TEXT,
            delivered_at TEXT,
            dedup_key    TEXT
        )
        "#,
    )
    .execute(&pool)
    .await?;
    add_columns_if_missing(
        &pool,
        "outbound_queue",
        &[("in_flight_at", "in_flight_at TEXT"), ("dedup_key", "dedup_key TEXT"), ("quoted_json", "quoted_json TEXT")],
    )
    .await?;
    add_columns_if_missing(&pool, "session_inbox", &[("dedup_key", "dedup_key TEXT")]).await?;
    for ddl in [
        "CREATE INDEX IF NOT EXISTS idx_outbound_status_enqueued ON outbound_queue(status, enqueued_at)",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_outbound_dedup ON outbound_queue(dedup_key) WHERE dedup_key IS NOT NULL",
        "CREATE INDEX IF NOT EXISTS idx_session_inbox_status ON session_inbox(status, id)",
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_session_inbox_dedup ON session_inbox(dedup_key) WHERE dedup_key IS NOT NULL",
    ] {
        sqlx::query(ddl).execute(&pool).await?;
    }
    Ok(pool)
}

async fn add_columns_if_missing(pool: &SqlitePool, table: &str, cols: &[(&str, &str)]) -> Result<()> {
    let have: Vec<String> = sqlx::query_scalar(&format!("SELECT name FROM pragma_table_info('{table}')"))
        .fetch_all(pool)
        .await?;
    for (name, ddl) in cols {
        if !have.iter().any(|h| h == name) {
            sqlx::query(&format!("ALTER TABLE {table} ADD COLUMN {ddl}")).execute(pool).await?;
        }
    }
    Ok(())
}

/// Queue a text message for the bot to send. `target` is a digit string, a
/// JID or [`INBOX_CHAT_OPERATOR_DM`]; the bot's drain re-checks it against
/// its allowlist.
pub async fn enqueue_text(pool: &SqlitePool, target: &str, body: &str, source: &str) -> Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "INSERT INTO outbound_queue (target, body, source, enqueued_at, status, attempts)
         VALUES (?1, ?2, ?3, ?4, 'pending', 0)
         RETURNING id",
    )
    .bind(target)
    .bind(body)
    .bind(source)
    .bind(crate::timestamp::now())
    .fetch_one(pool)
    .await
    .context("enqueue outbound whatsapp")?;
    Ok(row.0)
}

/// Queue a context message for the bot to type into a chat session. `sender`
/// is the attributed agent label, `body` the message without any header (the
/// bot adds the envelope). With a `dedup_key`, a second insert with the same
/// key is ignored and returns the first row's id.
pub async fn enqueue_inbox(
    pool: &SqlitePool,
    chat: &str,
    sender: &str,
    body: &str,
    source: &str,
    dedup_key: Option<&str>,
) -> Result<i64> {
    let mut conn = pool.acquire().await?;
    inbox_insert(&mut conn, chat, sender, body, source, dedup_key).await
}

async fn inbox_insert(
    conn: &mut sqlx::SqliteConnection,
    chat: &str,
    sender: &str,
    body: &str,
    source: &str,
    dedup_key: Option<&str>,
) -> Result<i64> {
    sqlx::query(
        "INSERT OR IGNORE INTO session_inbox
           (chat, sender, payload, source, enqueued_at, status, attempts, dedup_key)
         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', 0, ?6)",
    )
    .bind(chat)
    .bind(sender)
    .bind(body)
    .bind(source)
    .bind(crate::timestamp::now())
    .bind(dedup_key)
    .execute(&mut *conn)
    .await
    .context("enqueue whatsapp session inbox")?;
    let id: i64 = match dedup_key {
        Some(k) => sqlx::query_scalar("SELECT id FROM session_inbox WHERE dedup_key = ?1")
            .bind(k)
            .fetch_one(&mut *conn)
            .await?,
        None => sqlx::query_scalar("SELECT last_insert_rowid()").fetch_one(&mut *conn).await?,
    };
    Ok(id)
}

async fn outbound_insert(
    conn: &mut sqlx::SqliteConnection,
    target: &str,
    body: &str,
    source: &str,
    dedup_key: &str,
) -> Result<i64> {
    sqlx::query(
        "INSERT OR IGNORE INTO outbound_queue
           (target, body, source, enqueued_at, status, attempts, dedup_key)
         VALUES (?1, ?2, ?3, ?4, 'pending', 0, ?5)",
    )
    .bind(target)
    .bind(body)
    .bind(source)
    .bind(crate::timestamp::now())
    .bind(dedup_key)
    .execute(&mut *conn)
    .await
    .context("enqueue outbound whatsapp")?;
    Ok(sqlx::query_scalar("SELECT id FROM outbound_queue WHERE dedup_key = ?1")
        .bind(dedup_key)
        .fetch_one(&mut *conn)
        .await?)
}

/// One delivery to a WhatsApp chat: the visible message and, optionally, the
/// context message for the chat session. Both rows go in one transaction,
/// keyed by `dedup_key` (`<key>:message`, `<key>:context`), so a retried
/// delivery never duplicates either row and never leaves one without the
/// other. `chat` is the target of both rows. `message_attempt` > 0 queues
/// the visible message again under `<key>:message:r<n>`; the caller does
/// this only when the earlier row failed without being transmitted
/// ([`OutboundState::failed_untransmitted`]). The context row keeps its key
/// and is not repeated.
pub struct Delivery<'a> {
    pub chat: &'a str,
    pub message: &'a str,
    pub context: Option<(&'a str, &'a str)>, // (sender, body)
    pub source: &'a str,
    pub dedup_key: &'a str,
    pub message_attempt: u32,
}

/// Returns `(outbound id, inbox id)`.
pub async fn enqueue_delivery(pool: &SqlitePool, d: Delivery<'_>) -> Result<(i64, Option<i64>)> {
    let message_key = match d.message_attempt {
        0 => format!("{}:message", d.dedup_key),
        n => format!("{}:message:r{n}", d.dedup_key),
    };
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let out = outbound_insert(&mut tx, d.chat, d.message, d.source, &message_key).await?;
    let inbox = match d.context {
        Some((sender, body)) => Some(
            inbox_insert(&mut tx, d.chat, sender, body, d.source, Some(&format!("{}:context", d.dedup_key)))
                .await?,
        ),
        None => None,
    };
    tx.commit().await?;
    Ok((out, inbox))
}

/// What the tasks ledger reads of one outbound row.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct OutboundState {
    /// `pending`, `in_flight`, `sent` or `failed`.
    pub status: String,
    pub sent_at: Option<String>,
    pub last_error: Option<String>,
    /// The WhatsApp message id, fixed by the bot's drain immediately before
    /// its first send attempt (`markInFlight`). `None` proves the row was
    /// never handed to the socket: a row the drain refused (a target off the
    /// allowlist, a missing media file, a message the secret filter
    /// withheld) failed without any transmission.
    pub msg_id: Option<String>,
}

impl OutboundState {
    /// The row failed, and the failure proves the message never left this
    /// machine, so a new message cannot duplicate it.
    pub fn failed_untransmitted(&self) -> bool {
        self.status == "failed" && self.msg_id.is_none()
    }
}

/// One outbound row, or `None` when the row does not exist.
pub async fn outbound_status(pool: &SqlitePool, id: i64) -> Result<Option<OutboundState>> {
    Ok(sqlx::query_as("SELECT status, sent_at, last_error, msg_id FROM outbound_queue WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// Queue one text message under `dedup_key`: a second call with the same key
/// returns the first row's id and queues nothing.
pub async fn enqueue_text_once(pool: &SqlitePool, target: &str, body: &str, source: &str, dedup_key: &str) -> Result<i64> {
    let mut conn = pool.acquire().await?;
    outbound_insert(&mut conn, target, body, source, dedup_key).await
}

/// Digits of the user part of a WhatsApp id or phone number: a formatted
/// phone number, a `@s.whatsapp.net` JID with or without a `:<device>`
/// suffix, and an `@lid` id all reduce to their digits. Mirrors `normalizeSenderId` in messaging/whatsapp.
pub fn normalize_digits(id: &str) -> String {
    let user = id.split('@').next().unwrap_or("");
    let user = user.split(':').next().unwrap_or("");
    user.chars().filter(|c| c.is_ascii_digit()).collect()
}

/// The digit sets of `WHATSAPP_ALLOWED_DM_JIDS`.
pub fn allowed_dm_digits() -> Vec<String> {
    std::env::var("WHATSAPP_ALLOWED_DM_JIDS")
        .unwrap_or_default()
        .split(',')
        .map(|s| normalize_digits(s.trim().trim_matches('"')))
        .filter(|d| !d.is_empty())
        .collect()
}

/// Validate and canonicalize a DM chat reference for delivery. An `@lid`
/// chat is kept as is (a reply must go to the exact chat); any other form
/// becomes `<digits>@s.whatsapp.net`. The digits must be on the DM
/// allowlist.
pub fn canonical_dm_chat(chat: &str) -> Result<String> {
    canonical_dm_chat_in(chat, &allowed_dm_digits())
}

fn canonical_dm_chat_in(chat: &str, allowed: &[String]) -> Result<String> {
    if chat.contains("@g.us") {
        anyhow::bail!("{chat:?} is a group, not a WhatsApp DM chat");
    }
    let digits = normalize_digits(chat);
    if digits.len() < 8 {
        anyhow::bail!("{chat:?} is not a WhatsApp DM chat");
    }
    if !allowed.contains(&digits) {
        anyhow::bail!("{chat:?} is not on WHATSAPP_ALLOWED_DM_JIDS");
    }
    Ok(if chat.trim().ends_with("@lid") {
        format!("{digits}@lid")
    } else {
        format!("{digits}@s.whatsapp.net")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn queue_tables_accept_inserts_on_a_fresh_db() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        let pool = open(dir.path()).await.unwrap();
        let a = enqueue_text(&pool, "5511999999999", "hello", "test").await.unwrap();
        let b = enqueue_inbox(&pool, INBOX_CHAT_OPERATOR_DM, "task:ab12", "x", "test", None)
            .await
            .unwrap();
        assert!(a > 0 && b > 0);
        let (status,): (String,) =
            sqlx::query_as("SELECT status FROM session_inbox WHERE id = ?1")
                .bind(b)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(status, "pending");
    }

    #[tokio::test]
    async fn delivery_is_atomic_and_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        let pool = open(dir.path()).await.unwrap();
        let d = || Delivery {
            chat: "dm",
            message: "done",
            context: Some(("task:ab12cd34", "result")),
            source: "task:ab12cd34",
            dedup_key: "task:ab12cd34:done",
            message_attempt: 0,
        };
        let first = enqueue_delivery(&pool, d()).await.unwrap();
        let again = enqueue_delivery(&pool, d()).await.unwrap();
        assert_eq!(first, again, "a retried delivery returns the same rows");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outbound_queue").fetch_one(&pool).await.unwrap();
        let m: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session_inbox").fetch_one(&pool).await.unwrap();
        assert_eq!((n, m), (1, 1));
        // A redrive queues the visible message again and not the context.
        let redrive = enqueue_delivery(&pool, Delivery { message_attempt: 1, ..d() }).await.unwrap();
        assert_ne!(redrive.0, first.0);
        assert_eq!(redrive.1, first.1);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM outbound_queue").fetch_one(&pool).await.unwrap();
        let m: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session_inbox").fetch_one(&pool).await.unwrap();
        assert_eq!((n, m), (2, 1));
    }

    #[tokio::test]
    async fn open_heals_a_bot_created_table_without_the_new_columns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        let raw = crate::db::open(&dir.path().join(WHATSAPP_DB_PATH)).await.unwrap();
        sqlx::query(
            "CREATE TABLE session_inbox (id INTEGER PRIMARY KEY AUTOINCREMENT, chat TEXT NOT NULL,
             sender TEXT NOT NULL, payload TEXT NOT NULL, source TEXT NOT NULL, enqueued_at TEXT NOT NULL,
             status TEXT NOT NULL DEFAULT 'pending', attempts INTEGER NOT NULL DEFAULT 0,
             last_error TEXT, delivered_at TEXT)",
        )
        .execute(&raw)
        .await
        .unwrap();
        raw.close().await;
        let pool = open(dir.path()).await.unwrap();
        enqueue_inbox(&pool, "dm", "main", "x", "t", Some("k")).await.unwrap();
    }

    #[test]
    fn dm_chat_canonicalization() {
        assert_eq!(normalize_digits("+55 11 99999-9999"), "5511999999999");
        assert_eq!(normalize_digits("5511999999999:12@s.whatsapp.net"), "5511999999999");
        let allowed = vec!["5511999999999".to_string(), "123456789012".to_string()];
        let c = |s: &str| canonical_dm_chat_in(s, &allowed);
        assert_eq!(c("+55 11 99999-9999").unwrap(), "5511999999999@s.whatsapp.net");
        // Synthetic ids built at runtime (the committed-secrets scanner reads
        // a literal `<digits>@<domain>` as a real JID).
        let lid = format!("{}@lid", "123456789012");
        assert_eq!(c(&lid).unwrap(), lid);
        assert!(c(&format!("{}@s.whatsapp.net", "5511888888888")).is_err());
        assert!(c(&format!("{}@g.us", "120363000000000000")).is_err());
    }
}
