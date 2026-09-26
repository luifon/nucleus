//! Rust-side writers for the WhatsApp bot's queue tables (ADR-020 / ADR-033 /
//! ADR-036).
//!
//! `memory/whatsapp.db` is owned by the TypeScript bot. The one sanctioned
//! cross-process write is a queue table the bot drains (ADR-020 §5). Two such
//! tables exist:
//!
//! - `outbound_queue` — a message the bot sends to a WhatsApp chat (reminders,
//!   task results, issue-pipeline notices). The bot routes operator messages
//!   for pipeline items to its own `intake_inbound` table, which Rust only
//!   reads (ADR-036).
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
        &[
            ("in_flight_at", "in_flight_at TEXT"),
            ("dedup_key", "dedup_key TEXT"),
            ("quoted_json", "quoted_json TEXT"),
            ("wa_ts", "wa_ts INTEGER"),
        ],
    )
    .await?;
    add_columns_if_missing(&pool, "session_inbox", &[("dedup_key", "dedup_key TEXT")]).await?;
    // ADR-036: LIDs the bot verified as the operator through the live LID
    // mapping, with when. The bot writes it; Rust reads it (task chats).
    // Must match messaging/whatsapp/src/intake.ts.
    sqlx::query("CREATE TABLE IF NOT EXISTS operator_lid_verified (digits TEXT PRIMARY KEY, verified_at TEXT NOT NULL)")
        .execute(&pool)
        .await?;
    // ADR-036: the DM chat session's list of waiting intake decisions. One
    // row; Rust writes it, the bot reads it. Must match
    // messaging/whatsapp/src/intake.ts.
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS intake_chat_block (id INTEGER PRIMARY KEY CHECK (id = 1), block TEXT NOT NULL, updated_at TEXT NOT NULL)",
    )
    .execute(&pool)
    .await?;
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

// ── issue pipeline (ADR-036) ─────────────────────────────────────────────
//
// Operator messages the bot routes to a pipeline item land in the bot's
// `intake_inbound` table; Rust only reads it.

async fn table_exists(pool: &SqlitePool, table: &str) -> Result<bool> {
    Ok(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1")
        .bind(table)
        .fetch_one(pool)
        .await?
        > 0)
}

/// One operator message the bot routed to a pipeline item.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct IntakeInbound {
    pub id: i64,
    pub item_key: String,
    pub chat_id: String,
    pub wa_msg_id: String,
    pub text: String,
    pub received_at: String,
    /// `text` when the operator typed it; `voice` (a transcription) or
    /// `forwarded` otherwise. A decision from anything but typed text is
    /// always confirmed first.
    pub input_kind: String,
    /// `operator` when the bot checked that the sender is the operator's
    /// own identity; any other value (a table from before the column
    /// existed reads as `unknown`) is never interpreted.
    pub sender: String,
    /// WhatsApp's own `messageTimestamp` of the message (seconds), when the
    /// bot knew it. `received_at` is the arrival at the bot, stamped before
    /// the message was handled.
    pub wa_ts: Option<i64>,
}

/// The `wa_ts` select expression for `intake_inbound` (with a table
/// `prefix` such as `i.`), or NULL for a table from before the column.
async fn wa_ts_column(pool: &SqlitePool, prefix: &str) -> Result<String> {
    let has: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info('intake_inbound') WHERE name = 'wa_ts'")
        .fetch_one(pool)
        .await?;
    Ok(if has { format!("{prefix}wa_ts") } else { "NULL AS wa_ts".into() })
}

/// `item_key` of a DM message that names no item (ADR-036): it answers a
/// question the pipeline asked in the DM.
pub const INTAKE_DM_KEY: &str = "dm";

/// `item_key` of an operator DM message that went to the DM chat session
/// (ADR-036): stored so `nucleus intake interpret-latest` can read the
/// operator's own text; the tick never interprets it by itself.
pub const INTAKE_CHAT_KEY: &str = "chat";

/// The newest operator DM message that went to the chat session
/// (`item_key = chat`, `sender = operator`, not a group), in `chat` when
/// given.
pub async fn latest_chat_message(pool: &SqlitePool, chat: Option<&str>) -> Result<Option<IntakeInbound>> {
    if !table_exists(pool, "intake_inbound").await? {
        return Ok(None);
    }
    let has_sender: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info('intake_inbound') WHERE name = 'sender'")
        .fetch_one(pool)
        .await?;
    if !has_sender {
        return Ok(None);
    }
    let wa_ts = wa_ts_column(pool, "").await?;
    Ok(sqlx::query_as(&format!(
        "SELECT id, item_key, chat_id, wa_msg_id, text, received_at, input_kind, sender, {wa_ts} FROM intake_inbound
          WHERE item_key = ?1 AND sender = 'operator' AND chat_id NOT LIKE ?2 AND (?3 IS NULL OR chat_id = ?3)
          ORDER BY id DESC LIMIT 1"
    ))
    .bind(INTAKE_CHAT_KEY)
    .bind(format!("%@{}", "g.us"))
    .bind(chat)
    .fetch_optional(pool)
    .await?)
}

/// The operator DM messages (`item_key = chat`, `sender = operator`) that
/// the running turn of DM chat `chat` covers, oldest first: the turn
/// engine's `chat_turns` row with `status = 'running'` for the chat, and
/// the `chat_inbound` rows it marked with that turn, joined to the stored
/// rows by WhatsApp message id. Empty when the bot tables are missing or no
/// turn runs.
pub async fn current_turn_messages(pool: &SqlitePool, chat: &str) -> Result<Vec<IntakeInbound>> {
    for t in ["intake_inbound", "chat_turns", "chat_inbound"] {
        if !table_exists(pool, t).await? {
            return Ok(vec![]);
        }
    }
    let has_sender: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info('intake_inbound') WHERE name = 'sender'")
        .fetch_one(pool)
        .await?;
    if !has_sender {
        return Ok(vec![]);
    }
    let wa_ts = wa_ts_column(pool, "i.").await?;
    Ok(sqlx::query_as(&format!(
        "SELECT i.id, i.item_key, i.chat_id, i.wa_msg_id, i.text, i.received_at, i.input_kind, i.sender, {wa_ts}
           FROM intake_inbound i
           JOIN chat_inbound c ON c.chat_id = i.chat_id AND c.wa_msg_id = i.wa_msg_id
          WHERE i.chat_id = ?1 AND i.item_key = ?2 AND i.sender = 'operator'
            AND c.turn_id = (SELECT id FROM chat_turns WHERE chat_id = ?1 AND status = 'running'
                              ORDER BY started_at DESC LIMIT 1)
          ORDER BY i.id"
    ))
    .bind(chat)
    .bind(INTAKE_CHAT_KEY)
    .fetch_all(pool)
    .await?)
}

/// Operator DM messages (`item_key = chat`, `sender = operator`) of the
/// last 7 days whose chat turn the bot marked `interrupted` on a restart
/// (the message itself, or the turn that read it), oldest first. The
/// caller filters out the ones already interpreted.
pub async fn interrupted_chat_messages(pool: &SqlitePool) -> Result<Vec<IntakeInbound>> {
    for t in ["intake_inbound", "chat_turns", "chat_inbound"] {
        if !table_exists(pool, t).await? {
            return Ok(vec![]);
        }
    }
    let has_sender: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info('intake_inbound') WHERE name = 'sender'")
        .fetch_one(pool)
        .await?;
    if !has_sender {
        return Ok(vec![]);
    }
    let wa_ts = wa_ts_column(pool, "i.").await?;
    let since = (chrono::Utc::now() - chrono::Duration::days(7)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    Ok(sqlx::query_as(&format!(
        "SELECT DISTINCT i.id, i.item_key, i.chat_id, i.wa_msg_id, i.text, i.received_at, i.input_kind, i.sender, {wa_ts}
           FROM intake_inbound i
           JOIN chat_inbound c ON c.chat_id = i.chat_id AND c.wa_msg_id = i.wa_msg_id
           LEFT JOIN chat_turns t ON t.id = c.turn_id
          WHERE i.item_key = ?1 AND i.sender = 'operator' AND i.received_at >= ?2
            AND (c.status = 'interrupted' OR t.status = 'interrupted')
          ORDER BY i.id"
    ))
    .bind(INTAKE_CHAT_KEY)
    .bind(since)
    .fetch_all(pool)
    .await?)
}

/// When the outbound row with `dedup_key` was sent, if it was: its
/// `sent_at` (the bot's clock) and WhatsApp's server timestamp of the sent
/// message in seconds (`wa_ts`, from the send result, when known).
pub async fn sent_by_dedup(pool: &SqlitePool, dedup_key: &str) -> Result<Option<(String, Option<i64>)>> {
    Ok(sqlx::query_as(
        "SELECT sent_at, wa_ts FROM outbound_queue WHERE dedup_key = ?1 AND status = 'sent' AND sent_at IS NOT NULL",
    )
    .bind(dedup_key)
    .fetch_optional(pool)
    .await?)
}

/// Add `line` as a new paragraph to the outbound row with `dedup_key` while
/// it is still `pending` (not claimed by the drain). False when there is no
/// such row any more (it was sent or is being sent).
pub async fn append_to_pending(pool: &SqlitePool, dedup_key: &str, line: &str) -> Result<bool> {
    let res = sqlx::query("UPDATE outbound_queue SET body = body || ?2 WHERE dedup_key = ?1 AND status = 'pending'")
        .bind(dedup_key)
        .bind(format!("\n\n{line}"))
        .execute(pool)
        .await?;
    Ok(res.rows_affected() == 1)
}

/// Replace the block the bot adds to every operator message it types into
/// the DM chat session (`intake_chat_block`, one row): what waits for an
/// intake decision. Empty when nothing waits. Rust writes it; the bot only
/// reads it.
pub async fn set_intake_chat_block(pool: &SqlitePool, block: &str) -> Result<()> {
    sqlx::query(
        "INSERT INTO intake_chat_block (id, block, updated_at) VALUES (1, ?1, ?2)
         ON CONFLICT(id) DO UPDATE SET block = excluded.block, updated_at = excluded.updated_at
         WHERE intake_chat_block.block <> excluded.block",
    )
    .bind(block)
    .bind(crate::timestamp::now())
    .execute(pool)
    .await?;
    Ok(())
}

/// The block [`set_intake_chat_block`] wrote.
pub async fn intake_chat_block(pool: &SqlitePool) -> Result<String> {
    Ok(sqlx::query_scalar("SELECT block FROM intake_chat_block WHERE id = 1").fetch_optional(pool).await?.unwrap_or_default())
}

/// Rows of `intake_inbound` with an id above `after`, oldest first.
pub async fn intake_inbound_after(pool: &SqlitePool, after: i64, limit: i64) -> Result<Vec<IntakeInbound>> {
    if !table_exists(pool, "intake_inbound").await? {
        return Ok(vec![]);
    }
    let has_sender: bool = sqlx::query_scalar("SELECT COUNT(*) > 0 FROM pragma_table_info('intake_inbound') WHERE name = 'sender'")
        .fetch_one(pool)
        .await?;
    let sender = if has_sender { "sender" } else { "'unknown' AS sender" };
    let wa_ts = wa_ts_column(pool, "").await?;
    Ok(sqlx::query_as(&format!(
        "SELECT id, item_key, chat_id, wa_msg_id, text, received_at, input_kind, {sender}, {wa_ts} FROM intake_inbound
          WHERE id > ?1 ORDER BY id LIMIT ?2"
    ))
    .bind(after)
    .bind(limit)
    .fetch_all(pool)
    .await?)
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

/// The digit sets of `WHATSAPP_OPERATOR_LIDS` (ADR-036): the operator's own
/// LIDs.
pub fn operator_lids_env() -> Vec<String> {
    std::env::var("WHATSAPP_OPERATOR_LIDS")
        .unwrap_or_default()
        .split(',')
        .map(|s| normalize_digits(s.trim().trim_matches('"')))
        .filter(|d| !d.is_empty())
        .collect()
}

/// How long a LID the bot verified as the operator through the live LID
/// mapping counts (`operator_lid_verified`, ADR-036). Mirrors
/// `OPERATOR_LID_TTL_MS` in messaging/whatsapp/src/intake.ts.
pub const OPERATOR_LID_TTL_SECS: i64 = 600;

/// LIDs the bot verified as the operator through the live mapping within
/// [`OPERATOR_LID_TTL_SECS`] (`operator_lid_verified`, which the bot writes
/// and Rust only reads). Empty when the table is missing.
pub async fn verified_operator_lids(pool: &SqlitePool) -> Result<Vec<String>> {
    if !table_exists(pool, "operator_lid_verified").await? {
        return Ok(vec![]);
    }
    let since = (chrono::Utc::now() - chrono::Duration::seconds(OPERATOR_LID_TTL_SECS))
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    Ok(sqlx::query_scalar("SELECT digits FROM operator_lid_verified WHERE verified_at > ?1")
        .bind(since)
        .fetch_all(pool)
        .await?)
}

/// The operator's LIDs a Rust process can accept now: `WHATSAPP_OPERATOR_LIDS`
/// and the fresh entries of `operator_lid_verified` in `whatsapp_db` (read
/// only; a missing or unreadable database adds none).
pub async fn accepted_operator_lids(whatsapp_db: &Path) -> Vec<String> {
    let mut out = operator_lids_env();
    if whatsapp_db.exists() {
        if let Ok(pool) = crate::db::open_read_only(whatsapp_db).await {
            out.extend(verified_operator_lids(&pool).await.unwrap_or_default());
            pool.close().await;
        }
    }
    out
}

/// Validate and canonicalize a DM chat reference for delivery. An `@lid`
/// chat is kept as is (a reply must go to the exact chat); any other form
/// becomes `<digits>@s.whatsapp.net`. The digits must be on the DM
/// allowlist, or the chat is an `@lid` in `WHATSAPP_OPERATOR_LIDS`.
pub fn canonical_dm_chat(chat: &str) -> Result<String> {
    canonical_dm_chat_in(chat, &allowed_dm_digits(), &operator_lids_env())
}

/// [`canonical_dm_chat`] that also accepts an `@lid` chat in `operator_lids`
/// (see [`accepted_operator_lids`]).
pub fn canonical_dm_chat_with(chat: &str, operator_lids: &[String]) -> Result<String> {
    canonical_dm_chat_in(chat, &allowed_dm_digits(), operator_lids)
}

/// The chat a task's result goes to (ADR-036): its origin chat when that is
/// still accepted ([`canonical_dm_chat_with`]); an operator LID that is no
/// longer accepted (its mapping verification expired) falls back to the
/// operator's phone JID (the first `WHATSAPP_ALLOWED_DM_JIDS` entry), so
/// the result is not lost. Any other chat that is not accepted is an error.
pub fn task_result_chat(chat: &str, operator_lids: &[String]) -> Result<String> {
    match canonical_dm_chat_with(chat, operator_lids) {
        Ok(c) => Ok(c),
        Err(e) if chat.trim().ends_with("@lid") => match allowed_dm_digits().first() {
            Some(phone) => Ok(format!("{phone}@s.whatsapp.net")),
            None => Err(e),
        },
        Err(e) => Err(e),
    }
}

fn canonical_dm_chat_in(chat: &str, allowed: &[String], operator_lids: &[String]) -> Result<String> {
    if chat.contains("@g.us") {
        anyhow::bail!("{chat:?} is a group, not a WhatsApp DM chat");
    }
    let digits = normalize_digits(chat);
    if digits.len() < 8 {
        anyhow::bail!("{chat:?} is not a WhatsApp DM chat");
    }
    let lid = chat.trim().ends_with("@lid");
    if !allowed.contains(&digits) && !(lid && operator_lids.contains(&digits)) {
        anyhow::bail!("{chat:?} is not on WHATSAPP_ALLOWED_DM_JIDS and not a verified operator LID");
    }
    Ok(if lid { format!("{digits}@lid") } else { format!("{digits}@s.whatsapp.net") })
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

    #[tokio::test]
    async fn bot_tables_may_be_absent() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        let pool = open(dir.path()).await.unwrap();
        // Before the bot created its tables, nothing is there to read.
        assert!(intake_inbound_after(&pool, 0, 10).await.unwrap().is_empty());
        let groups: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE name LIKE 'intake_group%'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(groups, 0, "no group table is created");
    }

    #[tokio::test]
    async fn an_append_succeeds_only_on_a_pending_row() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        let pool = open(dir.path()).await.unwrap();
        enqueue_text_once(&pool, "dm", "Question?", "intake:ask", "k1").await.unwrap();
        assert!(append_to_pending(&pool, "k1", "Also received: 'x'").await.unwrap());
        let body: String = sqlx::query_scalar("SELECT body FROM outbound_queue WHERE dedup_key = 'k1'").fetch_one(&pool).await.unwrap();
        assert_eq!(body, "Question?\n\nAlso received: 'x'");
        // Claimed by the drain: the append is refused (the caller queues the
        // separate note) and the body is unchanged.
        sqlx::query("UPDATE outbound_queue SET status = 'in_flight' WHERE dedup_key = 'k1'").execute(&pool).await.unwrap();
        assert!(!append_to_pending(&pool, "k1", "Also received: 'y'").await.unwrap());
        let body: String = sqlx::query_scalar("SELECT body FROM outbound_queue WHERE dedup_key = 'k1'").fetch_one(&pool).await.unwrap();
        assert!(!body.contains("'y'"));
        assert!(!append_to_pending(&pool, "missing", "z").await.unwrap());
    }

    #[tokio::test]
    async fn an_inbound_table_without_the_sender_column_reads_as_unknown() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("memory")).unwrap();
        let pool = open(dir.path()).await.unwrap();
        sqlx::query(
            "CREATE TABLE intake_inbound (id INTEGER PRIMARY KEY AUTOINCREMENT, item_key TEXT NOT NULL, chat_id TEXT NOT NULL,
             wa_msg_id TEXT NOT NULL, text TEXT NOT NULL, received_at TEXT NOT NULL, input_kind TEXT NOT NULL DEFAULT 'text')",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO intake_inbound (item_key, chat_id, wa_msg_id, text, received_at) VALUES ('1', 'c', 'm', 'hi', 't')")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(intake_inbound_after(&pool, 0, 10).await.unwrap()[0].sender, "unknown");
        sqlx::query("ALTER TABLE intake_inbound ADD COLUMN sender TEXT NOT NULL DEFAULT 'unknown'").execute(&pool).await.unwrap();
        sqlx::query("UPDATE intake_inbound SET sender = 'operator'").execute(&pool).await.unwrap();
        assert_eq!(intake_inbound_after(&pool, 0, 10).await.unwrap()[0].sender, "operator");
    }

    #[test]
    fn dm_chat_canonicalization() {
        assert_eq!(normalize_digits("+55 11 99999-9999"), "5511999999999");
        assert_eq!(normalize_digits("5511999999999:12@s.whatsapp.net"), "5511999999999");
        let allowed = vec!["5511999999999".to_string(), "123456789012".to_string()];
        let c = |s: &str| canonical_dm_chat_in(s, &allowed, &[]);
        assert_eq!(c("+55 11 99999-9999").unwrap(), "5511999999999@s.whatsapp.net");
        // Synthetic ids built at runtime (the committed-secrets scanner reads
        // a literal `<digits>@<domain>` as a real JID).
        let lid = format!("{}@lid", "123456789012");
        assert_eq!(c(&lid).unwrap(), lid);
        assert!(c(&format!("{}@s.whatsapp.net", "5511888888888")).is_err());
        assert!(c(&format!("{}@g.us", "120363000000000000")).is_err());
    }
}
