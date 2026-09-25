// Uses node:sqlite (Node 22.5+, stable in Node 24). No native build needed.
import { DatabaseSync } from "node:sqlite";
import { randomUUID } from "node:crypto";
import fs from "node:fs";
import path from "node:path";

/** Idempotent column additions via PRAGMA table_info detection — TS has no
 *  migration runner, and ADR-020 called the tolerated-duplicate-error ALTER
 *  pattern an accumulation smell. Checking the actual schema is exact. */
export function addColumnsIfMissing(
  db: DatabaseSync,
  table: string,
  cols: Array<[name: string, ddl: string]>,
): void {
  const have = new Set(
    (db.prepare(`PRAGMA table_info(${table})`).all() as Array<{ name: string }>).map(
      (r) => r.name,
    ),
  );
  for (const [name, ddl] of cols) {
    if (!have.has(name)) db.exec(`ALTER TABLE ${table} ADD COLUMN ${ddl}`);
  }
}

export class ChatSessionStore {
  private db: DatabaseSync;

  constructor(dbPath: string) {
    fs.mkdirSync(path.dirname(dbPath), { recursive: true });
    this.db = new DatabaseSync(dbPath);
    this.db.exec(`PRAGMA journal_mode = WAL;`);
    this.db.exec(`
      CREATE TABLE IF NOT EXISTS chat_sessions (
        chat_id TEXT PRIMARY KEY,
        session_id TEXT NOT NULL,
        created_at TEXT NOT NULL,
        last_active TEXT NOT NULL,
        turns INTEGER NOT NULL DEFAULT 0
      );

      CREATE TABLE IF NOT EXISTS chat_state (
        chat_id TEXT PRIMARY KEY,
        members_seen TEXT NOT NULL,
        disabled INTEGER NOT NULL DEFAULT 0,
        disabled_reason TEXT,
        updated_at TEXT NOT NULL
      );

      CREATE TABLE IF NOT EXISTS pending_classifications (
        id TEXT PRIMARY KEY,
        chat_id TEXT NOT NULL,
        captured_at TEXT NOT NULL,
        capture_text TEXT NOT NULL,
        body TEXT NOT NULL,
        filename TEXT NOT NULL,
        options_json TEXT NOT NULL,
        status TEXT NOT NULL,
        resolved_at TEXT,
        resolved_bucket TEXT,
        resolved_path TEXT
      );

      CREATE INDEX IF NOT EXISTS idx_pending_chat_status
        ON pending_classifications(chat_id, status, captured_at DESC);

      -- Cross-process WhatsApp send queue. The reminders binary (and
      -- anyone else needing to send to WhatsApp from outside Alfred's
      -- process) inserts here; Alfred drains every 5s. Target is
      -- either a group NAME (resolved via Alfred's allowlist map) or
      -- a JID (used directly when it matches the allowlist).
      --
      -- ADR-018 media extension: kind ∈ {text, image, document}; body
      -- doubles as the caption for media rows. media_path MUST be a
      -- drain-owned staged file under memory/outbound-staging/ — the
      -- drain unlinks it at terminal state (sent or failed). Never point
      -- it at a document-library original.
      CREATE TABLE IF NOT EXISTS outbound_queue (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        target TEXT NOT NULL,
        body TEXT NOT NULL,
        source TEXT NOT NULL,
        enqueued_at TEXT NOT NULL,
        status TEXT NOT NULL DEFAULT 'pending',
        attempts INTEGER NOT NULL DEFAULT 0,
        last_error TEXT,
        sent_at TEXT,
        msg_id TEXT,
        kind TEXT NOT NULL DEFAULT 'text',
        media_path TEXT,
        mimetype TEXT,
        filename TEXT
      );

      CREATE INDEX IF NOT EXISTS idx_outbound_status_enqueued
        ON outbound_queue(status, enqueued_at);

      -- ADR-027: every connection close, classified. The churn-diagnosis
      -- dataset, populated passively by the breaker in index.ts.
      CREATE TABLE IF NOT EXISTS connection_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        ts TEXT NOT NULL,
        class TEXT NOT NULL,
        code INTEGER,
        uptime_ms INTEGER
      );

      -- ADR-005a: brain-dump review-before-apply. Each capture produces
      -- a plan that's held here until the operator approves (or rejects,
      -- or times out). One pending plan per chat at a time; new captures
      -- auto-expire prior ones.
      CREATE TABLE IF NOT EXISTS pending_plans (
        id            TEXT PRIMARY KEY,
        chat_id       TEXT NOT NULL,
        captured_at   TEXT NOT NULL,
        capture_text  TEXT NOT NULL,
        input_kind    TEXT NOT NULL,
        ops_json      TEXT NOT NULL,
        summary       TEXT NOT NULL,
        confidence    REAL NOT NULL,
        status        TEXT NOT NULL,
        resolved_at   TEXT,
        resolution    TEXT
      );

      CREATE INDEX IF NOT EXISTS idx_pending_plans_chat_status_time
        ON pending_plans(chat_id, status, captured_at DESC);

      -- ADR-033: context messages other processes want typed into one of
      -- this bot's chat sessions (task results, session-send --to
      -- whatsapp-dm). Queue table owned by the bot (ADR-020 §5); Rust
      -- producers insert via nucleus_core::whatsapp_queue. chat = 'dm'
      -- means the operator's DM chat, otherwise it is the exact chat JID.
      -- payload is the body only: the bot builds the attribution envelope
      -- from sender when it types the message. dedup_key makes a
      -- producer's retry idempotent.
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
        delivered_at TEXT
      );
      CREATE INDEX IF NOT EXISTS idx_session_inbox_status
        ON session_inbox(status, id);

      -- ADR-033: every conversational message and every session turn, so
      -- the state of a conversation is visible and a restart can report
      -- what it interrupted. ref = the marker typed with the message.
      CREATE TABLE IF NOT EXISTS chat_inbound (
        ref          TEXT PRIMARY KEY,
        chat_id      TEXT NOT NULL,
        pool         TEXT NOT NULL,
        wa_msg_id    TEXT,
        quoted_json  TEXT,
        input_kind   TEXT NOT NULL,
        text_preview TEXT NOT NULL,
        received_at  TEXT NOT NULL,
        typed_at     TEXT,
        turn_id      TEXT,
        status       TEXT NOT NULL,
        acked_at     TEXT,
        answered_at  TEXT,
        retried      INTEGER NOT NULL DEFAULT 0,
        error        TEXT
      );
      CREATE INDEX IF NOT EXISTS idx_chat_inbound_chat_status
        ON chat_inbound(chat_id, status, received_at);

      -- ADR-033: WhatsApp message ids already handled, per chat. A message
      -- Baileys delivers again (after a reconnect) is dropped before any
      -- action.
      CREATE TABLE IF NOT EXISTS seen_messages (
        chat_id   TEXT NOT NULL,
        wa_msg_id TEXT NOT NULL,
        seen_at   TEXT NOT NULL,
        PRIMARY KEY (chat_id, wa_msg_id)
      );

      -- ADR-033: task scopes. Each DM chat session runs with
      -- NUCLEUS_TASK_SCOPE=<random token>; the tasks CLI maps sha256(token)
      -- to the chat and limits the session to that chat's tasks. Cleared at
      -- boot (every session is respawned with a new token).
      CREATE TABLE IF NOT EXISTS task_scopes (
        token_sha256 TEXT PRIMARY KEY,
        chat_id      TEXT NOT NULL,
        created_at   TEXT NOT NULL
      );

      CREATE TABLE IF NOT EXISTS chat_turns (
        id                TEXT PRIMARY KEY,
        chat_id           TEXT NOT NULL,
        pool              TEXT NOT NULL,
        session_id        TEXT,
        kind              TEXT NOT NULL,
        status            TEXT NOT NULL,
        started_at        TEXT NOT NULL,
        ended_at          TEXT,
        ack_sent          INTEGER NOT NULL DEFAULT 0,
        progress_count    INTEGER NOT NULL DEFAULT 0,
        final_outbound_id INTEGER,
        reply_chars       INTEGER,
        error             TEXT
      );
      CREATE INDEX IF NOT EXISTS idx_chat_turns_chat_started
        ON chat_turns(chat_id, started_at DESC);
    `);
    // ADR-018: heal pre-media DBs. Fresh installs get the full shape from
    // the CREATE above; existing DBs gain the columns here.
    addColumnsIfMissing(this.db, "outbound_queue", [
      ["kind", "kind TEXT NOT NULL DEFAULT 'text'"],
      ["media_path", "media_path TEXT"],
      ["mimetype", "mimetype TEXT"],
      ["filename", "filename TEXT"],
      // ADR-033: the Baileys message a reply quotes (BufferJSON-encoded
      // {key, message}); null for unquoted rows.
      ["quoted_json", "quoted_json TEXT"],
      // ADR-033 send idempotency: set when a send starts (with msg_id =
      // the WhatsApp message id every attempt of the row reuses).
      ["in_flight_at", "in_flight_at TEXT"],
      ["dedup_key", "dedup_key TEXT"],
    ]);
    addColumnsIfMissing(this.db, "session_inbox", [["dedup_key", "dedup_key TEXT"]]);
    addColumnsIfMissing(this.db, "chat_inbound", [
      // The complete message text; text_preview stays the short form the
      // dashboard shows. A retry replays `text`, never the preview.
      ["text", "text TEXT"],
      // How the message arrived in the session: the transcript record's
      // promptSource ("typed"; null = queued, not known), and how many typed
      // chunks were not echoed in time (claude_session.ts flow control).
      ["prompt_source", "prompt_source TEXT"],
      ["typing_stalls", "typing_stalls INTEGER NOT NULL DEFAULT 0"],
    ]);
    addColumnsIfMissing(this.db, "seen_messages", [
      // received: the bot started handling the message; handled: its
      // durable hand-off (chat_inbound row, job, capture) completed. Rows
      // from before the column existed were handled.
      ["status", "status TEXT NOT NULL DEFAULT 'handled'"],
    ]);
    addColumnsIfMissing(this.db, "pending_plans", [
      // ADR-033 inbound idempotency. The WhatsApp message a plan was
      // planned from, and the message that resolved it (with the action),
      // so a message handled again after a crash finds its own plan
      // instead of planning or applying a second time.
      ["source_msg_id", "source_msg_id TEXT"],
      ["resolved_by_msg", "resolved_by_msg TEXT"],
      ["resolved_action", "resolved_action TEXT"],
      // Apply progress: the accepted op ids, each finished op's result
      // (op id → AppliedOp), and the final outcome sent to the chat.
      ["apply_ids_json", "apply_ids_json TEXT"],
      ["apply_progress_json", "apply_progress_json TEXT"],
      ["outcome_json", "outcome_json TEXT"],
    ]);
    addColumnsIfMissing(this.db, "chat_turns", [
      // The operator message an autonomous turn answers (a background
      // command it started finished), so a restart can report the turn.
      ["quote_ref", "quote_ref TEXT"],
      // Background commands still running when the turn ended.
      ["pending_bg", "pending_bg INTEGER"],
      // 1 when an operator message of the turn arrived as pasted content.
      ["pasted_input", "pasted_input INTEGER NOT NULL DEFAULT 0"],
    ]);
    this.db.exec(`
      CREATE UNIQUE INDEX IF NOT EXISTS idx_outbound_dedup
        ON outbound_queue(dedup_key) WHERE dedup_key IS NOT NULL;
      CREATE INDEX IF NOT EXISTS idx_outbound_msg_id ON outbound_queue(msg_id);
      CREATE UNIQUE INDEX IF NOT EXISTS idx_session_inbox_dedup
        ON session_inbox(dedup_key) WHERE dedup_key IS NOT NULL;
      CREATE UNIQUE INDEX IF NOT EXISTS idx_chat_inbound_wa_msg
        ON chat_inbound(chat_id, wa_msg_id) WHERE wa_msg_id IS NOT NULL;
      CREATE UNIQUE INDEX IF NOT EXISTS idx_pending_plans_source_msg
        ON pending_plans(chat_id, source_msg_id) WHERE source_msg_id IS NOT NULL;
      CREATE INDEX IF NOT EXISTS idx_pending_plans_resolved_by
        ON pending_plans(chat_id, resolved_by_msg) WHERE resolved_by_msg IS NOT NULL;
    `);
  }

  /** Most recently active chat whose id normalizes to one of `digits` — the
   *  chat key the operator's DM currently runs under (@s.whatsapp.net or
   *  @lid). */
  latestChatAmong(match: (chatId: string) => boolean): string | null {
    const rows = this.db
      .prepare("SELECT chat_id FROM chat_sessions ORDER BY last_active DESC")
      .all() as Array<{ chat_id: string }>;
    return rows.find((r) => match(r.chat_id))?.chat_id ?? null;
  }

  lookup(chatId: string): string | null {
    const row = this.db
      .prepare("SELECT session_id FROM chat_sessions WHERE chat_id = ?")
      .get(chatId) as { session_id: string } | undefined;
    return row?.session_id ?? null;
  }

  /** ADR-027: record one classified connection close. */
  recordConnectionEvent(cls: string, code: number | undefined, uptimeMs: number | null): void {
    this.db
      .prepare(`INSERT INTO connection_events (ts, class, code, uptime_ms) VALUES (?, ?, ?, ?)`)
      .run(new Date().toISOString(), cls, code ?? null, uptimeMs ?? null);
  }

  save(chatId: string, sessionId: string, isNew: boolean): void {
    const now = new Date().toISOString();
    if (isNew) {
      this.db
        .prepare(
          `INSERT OR REPLACE INTO chat_sessions
           (chat_id, session_id, created_at, last_active, turns)
           VALUES (?, ?, ?, ?, 1)`,
        )
        .run(chatId, sessionId, now, now);
    } else {
      this.db
        .prepare(
          `UPDATE chat_sessions
           SET session_id = ?, last_active = ?, turns = turns + 1
           WHERE chat_id = ?`,
        )
        .run(sessionId, now, chatId);
    }
  }

  /** Track group membership; if it grows, flip disabled and require manual re-enable. */
  observeMembers(chatId: string, memberIds: string[]): { disabled: boolean; reason?: string } {
    const sorted = [...memberIds].sort();
    const json = JSON.stringify(sorted);
    const now = new Date().toISOString();
    const prev = this.db
      .prepare("SELECT members_seen, disabled, disabled_reason FROM chat_state WHERE chat_id = ?")
      .get(chatId) as
      | { members_seen: string; disabled: number; disabled_reason: string | null }
      | undefined;

    if (!prev) {
      this.db
        .prepare(
          `INSERT INTO chat_state (chat_id, members_seen, disabled, updated_at) VALUES (?, ?, 0, ?)`,
        )
        .run(chatId, json, now);
      return { disabled: false };
    }
    if (prev.disabled) {
      return { disabled: true, reason: prev.disabled_reason ?? "manually disabled" };
    }
    if (prev.members_seen !== json) {
      const reason = `member list changed: was ${prev.members_seen}, now ${json}`;
      this.db
        .prepare(
          `UPDATE chat_state SET members_seen = ?, disabled = 1, disabled_reason = ?, updated_at = ? WHERE chat_id = ?`,
        )
        .run(json, reason, now, chatId);
      return { disabled: true, reason };
    }
    return { disabled: false };
  }
}

export interface ClassificationOption {
  /** Display label shown to user, e.g. "4-Areas/Nucleus". Also serves as
   *  the bucket path. */
  label: string;
  bucket: string;
}

export interface PendingClassification {
  id: string;
  chatId: string;
  capturedAt: string;
  captureText: string;
  body: string;
  filename: string;
  options: ClassificationOption[];
}

/** Storage for brain-dump captures whose classification confidence was too
 *  low to file blindly. The bot sends a "where does this go?" question to
 *  the user, who replies with a number; we look up the most recent pending
 *  in that chat and resolve it.
 *
 *  Opens its own connection to the same SQLite file as ChatSessionStore
 *  (memory/whatsapp.db). WAL mode handles concurrent connections fine.
 *  The pending_classifications schema is created by ChatSessionStore's
 *  constructor — make sure you instantiate ChatSessionStore first. */
export class PendingStore {
  private db: DatabaseSync;

  constructor(dbPath: string) {
    this.db = new DatabaseSync(dbPath);
    this.db.exec(`PRAGMA journal_mode = WAL;`);
  }

  insert(input: {
    chatId: string;
    captureText: string;
    body: string;
    filename: string;
    options: ClassificationOption[];
  }): string {
    const id = randomUUID();
    const now = new Date().toISOString();
    this.db
      .prepare(
        `INSERT INTO pending_classifications
         (id, chat_id, captured_at, capture_text, body, filename, options_json, status)
         VALUES (?, ?, ?, ?, ?, ?, ?, 'pending')`,
      )
      .run(
        id,
        input.chatId,
        now,
        input.captureText,
        input.body,
        input.filename,
        JSON.stringify(input.options),
      );
    return id;
  }

  /** Look up the most recently created `pending` row for this chat, if any. */
  mostRecentPending(chatId: string): PendingClassification | null {
    const row = this.db
      .prepare(
        `SELECT id, chat_id, captured_at, capture_text, body, filename, options_json
         FROM pending_classifications
         WHERE chat_id = ? AND status = 'pending'
         ORDER BY captured_at DESC LIMIT 1`,
      )
      .get(chatId) as
      | {
          id: string;
          chat_id: string;
          captured_at: string;
          capture_text: string;
          body: string;
          filename: string;
          options_json: string;
        }
      | undefined;
    if (!row) return null;
    return {
      id: row.id,
      chatId: row.chat_id,
      capturedAt: row.captured_at,
      captureText: row.capture_text,
      body: row.body,
      filename: row.filename,
      options: JSON.parse(row.options_json),
    };
  }

  markResolved(id: string, bucket: string, filedPath: string): void {
    const now = new Date().toISOString();
    this.db
      .prepare(
        `UPDATE pending_classifications
         SET status = 'resolved', resolved_at = ?, resolved_bucket = ?, resolved_path = ?
         WHERE id = ?`,
      )
      .run(now, bucket, filedPath, id);
  }

  /** Sweep `pending` rows older than `maxAgeMs` to status 'expired'. Returns
   *  the count expired. The expired rows still hold the body so a future
   *  manual recovery is possible. */
  expireOlderThan(maxAgeMs: number): number {
    const cutoff = new Date(Date.now() - maxAgeMs).toISOString();
    const res = this.db
      .prepare(
        `UPDATE pending_classifications
         SET status = 'expired'
         WHERE status = 'pending' AND captured_at < ?`,
      )
      .run(cutoff);
    return Number(res.changes ?? 0);
  }
}

export type OutboundKind = "text" | "image" | "document";

export interface OutboundRow {
  id: number;
  target: string;
  /** Message text; for media rows this is the caption (may be empty). */
  body: string;
  source: string;
  enqueuedAt: string;
  attempts: number;
  kind: OutboundKind;
  /** Drain-owned staged file (memory/outbound-staging/) — unlinked at
   *  terminal state. NEVER a document-library original. Null for text. */
  mediaPath: string | null;
  mimetype: string | null;
  filename: string | null;
  /** ADR-033: BufferJSON-encoded {key, message} this row replies to. */
  quotedJson: string | null;
  /** WhatsApp message id every send attempt of this row uses; null until
   *  the first attempt. */
  msgId: string | null;
}

/** How long an in-flight row waits before the drain may attempt it again.
 *  Covers the send timeout, a reconnect and the server acknowledgement. */
export const IN_FLIGHT_GRACE_MS = 120_000;

/** Outbound WhatsApp send queue. The reminders binary (and anyone else
 *  who needs to send a WhatsApp message from outside the bot's process)
 *  inserts rows here. The bot's main process drains every 1s, resolves
 *  `target` to a JID via the allowlist, and sends via Baileys.
 *
 *  `target` is either a group name ("Alfred", "Brain Dump") OR a raw
 *  JID. The drainer accepts both — but only if the resolved JID
 *  is on the allowlist (no sending to arbitrary chats).
 *
 *  Failures bump `attempts`; after a max-attempts threshold, status
 *  moves to 'failed' to stop retry storms.
 *
 *  Send idempotency (ADR-033): before a send the drain marks the row
 *  `in_flight` and fixes its WhatsApp message id (`msg_id`). The row is not
 *  retried while the send can still succeed — only after
 *  IN_FLIGHT_GRACE_MS and only when no send of it is still pending in this
 *  process. Every attempt reuses the same message id, so WhatsApp treats a
 *  retry of a message that did arrive as the same message, and a server
 *  acknowledgement for that id (after a reconnect too) marks the row sent. */
export class OutboundQueueStore {
  private db: DatabaseSync;

  constructor(dbPath: string) {
    this.db = new DatabaseSync(dbPath);
    this.db.exec(`PRAGMA journal_mode = WAL;`);
  }

  /** Insert a new pending row. Returns the auto-assigned id. `target` is
   *  either a group name (resolved via the bot's allowlist name→JID map)
   *  or a raw JID; either way the drainer authorizes before sending.
   *  Media rows (kind image/document) must set mediaPath to a drain-owned
   *  staged file — see the schema comment. */
  enqueue(input: {
    target: string;
    body: string;
    source: string;
    kind?: OutboundKind;
    mediaPath?: string;
    mimetype?: string;
    filename?: string;
    quotedJson?: string | null;
    /** A second enqueue with the same key is ignored (returns the first id). */
    dedupKey?: string | null;
  }): number {
    const now = new Date().toISOString();
    const res = this.db
      .prepare(
        `INSERT OR IGNORE INTO outbound_queue
           (target, body, source, enqueued_at, status, attempts,
            kind, media_path, mimetype, filename, quoted_json, dedup_key)
         VALUES (?, ?, ?, ?, 'pending', 0, ?, ?, ?, ?, ?, ?)`,
      )
      .run(
        input.target,
        input.body,
        input.source,
        now,
        input.kind ?? "text",
        input.mediaPath ?? null,
        input.mimetype ?? null,
        input.filename ?? null,
        input.quotedJson ?? null,
        input.dedupKey ?? null,
      );
    if (Number(res.changes) === 0 && input.dedupKey) {
      const row = this.db
        .prepare(`SELECT id FROM outbound_queue WHERE dedup_key = ?`)
        .get(input.dedupKey) as { id: number };
      return row.id;
    }
    return Number(res.lastInsertRowid);
  }

  /** Up-to-`limit` rows to send now, oldest first: pending rows, and
   *  in-flight rows older than IN_FLIGHT_GRACE_MS whose id is not in
   *  `active` (sends of this process that have not settled). */
  pending(limit: number = 20, active: ReadonlySet<number> = new Set(), nowMs = Date.now()): OutboundRow[] {
    const cutoff = new Date(nowMs - IN_FLIGHT_GRACE_MS).toISOString();
    const rows = (
      this.db
        .prepare(
          `SELECT id, target, body, source, enqueued_at, attempts,
                  kind, media_path, mimetype, filename, quoted_json, msg_id
             FROM outbound_queue
            WHERE status = 'pending'
               OR (status = 'in_flight' AND in_flight_at < ?)
            ORDER BY enqueued_at ASC, id ASC
            LIMIT ?`,
        )
        .all(cutoff, limit + active.size) as any[]
    )
      .filter((r) => !active.has(r.id))
      .slice(0, limit) as Array<{
        id: number;
        target: string;
        body: string;
        source: string;
        enqueued_at: string;
        attempts: number;
        kind: string;
        media_path: string | null;
        mimetype: string | null;
        filename: string | null;
        quoted_json: string | null;
        msg_id: string | null;
      }>;
    return rows.map((r) => ({
      id: r.id,
      target: r.target,
      body: r.body,
      source: r.source,
      enqueuedAt: r.enqueued_at,
      attempts: r.attempts,
      kind: (r.kind as OutboundKind) ?? "text",
      mediaPath: r.media_path,
      mimetype: r.mimetype,
      filename: r.filename,
      quotedJson: r.quoted_json,
      msgId: r.msg_id,
    }));
  }

  /** Start a send attempt: status in_flight, and the row's WhatsApp message
   *  id fixed (the first attempt stores `msgId`; later attempts keep the
   *  stored one). Returns the id to send with. */
  markInFlight(id: number, msgId: string): string {
    this.db
      .prepare(
        `UPDATE outbound_queue
            SET status = 'in_flight', in_flight_at = ?, msg_id = COALESCE(msg_id, ?)
          WHERE id = ? AND status IN ('pending','in_flight')`,
      )
      .run(new Date().toISOString(), msgId, id);
    const row = this.db.prepare(`SELECT msg_id FROM outbound_queue WHERE id = ?`).get(id) as
      | { msg_id: string | null }
      | undefined;
    return row?.msg_id ?? msgId;
  }

  markSent(id: number, msgId: string): void {
    const now = new Date().toISOString();
    this.db
      .prepare(
        `UPDATE outbound_queue
            SET status = 'sent', sent_at = COALESCE(sent_at, ?), msg_id = COALESCE(NULLIF(?, ''), msg_id)
          WHERE id = ?`,
      )
      .run(now, msgId, id);
  }

  /** A server acknowledgement for WhatsApp message `msgId` arrived: the row
   *  that owns that id was delivered. Returns the row id, if any. */
  markSentByMsgId(msgId: string): number | null {
    const row = this.db
      .prepare(`SELECT id FROM outbound_queue WHERE msg_id = ? AND status IN ('in_flight','pending','failed')`)
      .get(msgId) as { id: number } | undefined;
    if (!row) return null;
    this.markSent(row.id, msgId);
    return row.id;
  }

  /** Status of one row (tests, reconciliation). */
  status(id: number): string | null {
    const row = this.db.prepare(`SELECT status FROM outbound_queue WHERE id = ?`).get(id) as
      | { status: string }
      | undefined;
    return row?.status ?? null;
  }

  /** Record a delivery failure. After `maxAttempts` we stop retrying.
   *  Returns the resulting status so the drain can gate filesystem
   *  actions (staged-media unlink) on TERMINALITY — unlinking on a
   *  non-terminal failure would strand the retry (ADR-018). */
  markFailure(
    id: number,
    error: string,
    maxAttempts: number,
  ): { status: "pending" | "failed" } {
    const row = this.db
      .prepare(
        `SELECT attempts FROM outbound_queue WHERE id = ?`,
      )
      .get(id) as { attempts: number } | undefined;
    const attempts = (row?.attempts ?? 0) + 1;
    const status: "pending" | "failed" =
      attempts >= maxAttempts ? "failed" : "pending";
    this.db
      .prepare(
        `UPDATE outbound_queue
            SET attempts = ?, last_error = ?, status = ?
          WHERE id = ? AND status IN ('pending','in_flight')`,
      )
      .run(attempts, error, status, id);
    return { status };
  }

  /** A send attempt timed out. The row stays in_flight — the send can still
   *  succeed — and becomes retryable after IN_FLIGHT_GRACE_MS; the attempt
   *  counts toward `maxAttempts`. */
  markTimedOut(id: number, error: string, maxAttempts: number): { status: "in_flight" | "failed" } {
    const row = this.db.prepare(`SELECT attempts FROM outbound_queue WHERE id = ?`).get(id) as
      | { attempts: number }
      | undefined;
    const attempts = (row?.attempts ?? 0) + 1;
    const status = attempts >= maxAttempts ? "failed" : "in_flight";
    this.db
      .prepare(
        `UPDATE outbound_queue SET attempts = ?, last_error = ?, status = ?
          WHERE id = ? AND status = 'in_flight'`,
      )
      .run(attempts, error, status, id);
    return { status };
  }

  /** Terminal failure regardless of attempts — for non-retryable errors
   *  (missing/oversized media file): retrying can never succeed. */
  markFailedTerminal(id: number, error: string): void {
    this.db
      .prepare(
        `UPDATE outbound_queue
            SET attempts = attempts + 1, last_error = ?, status = 'failed'
          WHERE id = ?`,
      )
      .run(error, id);
  }

  /** media_paths of all still-pending rows — the boot sweep keeps these
   *  and deletes any other file in the staging dir. */
  pendingMediaPaths(): string[] {
    const rows = this.db
      .prepare(
        `SELECT media_path FROM outbound_queue
          WHERE status IN ('pending','in_flight') AND media_path IS NOT NULL`,
      )
      .all() as Array<{ media_path: string }>;
    return rows.map((r) => r.media_path);
  }
}

// ============================================================
// PendingPlansStore — ADR-005a brain-dump review-before-apply
// ============================================================

export type PlanStatus =
  | "pending"
  /** Accepted ops are being filed (resumable: see applyProgress). */
  | "applying"
  | "applied"
  | "partial"
  | "rejected"
  | "expired";

export interface PendingPlanRow {
  id: string;
  chatId: string;
  capturedAt: string;
  captureText: string;
  inputKind: "text" | "voice";
  opsJson: string;
  summary: string;
  confidence: number;
  status: PlanStatus;
  /** WhatsApp message the plan was planned from (null: none recorded). */
  sourceMsgId: string | null;
  /** WhatsApp message that resolved or is applying the plan. */
  resolvedByMsg: string | null;
  /** What that message did: apply | reject | supersede. */
  resolvedAction: PlanAction | null;
  /** Op ids accepted for filing, once applying started. */
  applyIds: number[] | null;
  /** Result of every op already filed, by op id (0 = the fallback op). */
  applyProgress: Record<string, unknown>;
  /** The outcome of a finished apply (JSON), for re-sending its reply. */
  outcomeJson: string | null;
}

/** What an operator message did to a plan. */
export type PlanAction = "apply" | "reject" | "supersede";

/** Storage for brain-dump plans that are pending operator review. The
 *  WhatsApp handler inserts on plan computation, looks up on operator
 *  reply, and marks resolved (applied/partial/rejected/expired) when
 *  the lifecycle ends. Schema is created by ChatSessionStore's CREATE
 *  block — instantiate that first. */
export class PendingPlansStore {
  private db: DatabaseSync;

  constructor(dbPath: string) {
    this.db = new DatabaseSync(dbPath);
    this.db.exec(`PRAGMA journal_mode = WAL;`);
  }

  insert(input: {
    chatId: string;
    captureText: string;
    inputKind: "text" | "voice";
    opsJson: string;
    summary: string;
    confidence: number;
    /** The WhatsApp message planned from. A second insert for the same
     *  message returns the first plan's id. */
    sourceMsgId?: string | null;
  }): string {
    const id = randomUUID();
    const now = new Date().toISOString();
    const res = this.db
      .prepare(
        `INSERT OR IGNORE INTO pending_plans
         (id, chat_id, captured_at, capture_text, input_kind,
          ops_json, summary, confidence, status, source_msg_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?)`,
      )
      .run(
        id,
        input.chatId,
        now,
        input.captureText,
        input.inputKind,
        input.opsJson,
        input.summary,
        input.confidence,
        input.sourceMsgId ?? null,
      );
    if (Number(res.changes) === 0 && input.sourceMsgId) {
      return this.bySourceMsg(input.chatId, input.sourceMsgId)!.id;
    }
    return id;
  }

  /** The plan planned from WhatsApp message `msgId` of `chatId`. */
  bySourceMsg(chatId: string, msgId: string): PendingPlanRow | null {
    const row = this.db
      .prepare(`SELECT * FROM pending_plans WHERE chat_id = ? AND source_msg_id = ?`)
      .get(chatId, msgId);
    return row ? rowToPlan(row) : null;
  }

  /** Plans that WhatsApp message `msgId` of `chatId` resolved or is
   *  applying, oldest first. */
  resolvedByMsg(chatId: string, msgId: string): PendingPlanRow[] {
    return (
      this.db
        .prepare(
          `SELECT * FROM pending_plans WHERE chat_id = ? AND resolved_by_msg = ?
            ORDER BY captured_at`,
        )
        .all(chatId, msgId) as any[]
    ).map(rowToPlan);
  }

  /** Start filing a plan: status applying, the accepted ids and the
   *  message that accepted them recorded before the first op is filed. */
  beginApply(id: string, ids: number[], byMsgId: string | null): void {
    this.db
      .prepare(
        `UPDATE pending_plans
            SET status = 'applying', apply_ids_json = ?, apply_progress_json = '{}',
                resolved_by_msg = ?, resolved_action = 'apply'
          WHERE id = ?`,
      )
      .run(JSON.stringify(ids), byMsgId, id);
  }

  /** Record the result of one filed op (op id; 0 = the fallback op). */
  recordApplied(id: string, opId: number, result: unknown): void {
    const row = this.get(id);
    if (!row) return;
    const progress = { ...row.applyProgress, [String(opId)]: result };
    this.db
      .prepare(`UPDATE pending_plans SET apply_progress_json = ? WHERE id = ?`)
      .run(JSON.stringify(progress), id);
  }

  get(id: string): PendingPlanRow | null {
    const row = this.db
      .prepare(`SELECT * FROM pending_plans WHERE id = ?`)
      .get(id) as any;
    if (!row) return null;
    return rowToPlan(row);
  }

  /** Most recent pending plan for this chat, if any. */
  mostRecentPending(chatId: string): PendingPlanRow | null {
    const row = this.db
      .prepare(
        `SELECT * FROM pending_plans
         WHERE chat_id = ? AND status = 'pending'
         ORDER BY captured_at DESC LIMIT 1`,
      )
      .get(chatId) as any;
    if (!row) return null;
    return rowToPlan(row);
  }

  /** Replace the persisted ops for a pending plan. Used when the operator
   *  course-corrects an op during review (a `modify` interpretation): the
   *  patched ops are what we actually apply, so persist them for audit. */
  updateOps(id: string, opsJson: string): void {
    this.db
      .prepare(`UPDATE pending_plans SET ops_json = ? WHERE id = ?`)
      .run(opsJson, id);
  }

  /** Set terminal status with resolution note. `by`: the WhatsApp
   *  message that resolved the plan and what it did; `outcomeJson`: the
   *  outcome of a finished apply. */
  resolve(
    id: string,
    status: PlanStatus,
    resolution: string,
    by?: { msgId: string | null; action: PlanAction } | null,
    outcomeJson?: string | null,
  ): void {
    const now = new Date().toISOString();
    this.db
      .prepare(
        `UPDATE pending_plans
            SET status = ?, resolved_at = ?, resolution = ?,
                resolved_by_msg = COALESCE(?, resolved_by_msg),
                resolved_action = COALESCE(?, resolved_action),
                outcome_json = COALESCE(?, outcome_json)
          WHERE id = ?`,
      )
      .run(status, now, resolution, by?.msgId ?? null, by?.msgId ? by.action : null, outcomeJson ?? null, id);
  }

  /** Expire all `pending` rows for this chat. Used on new-capture arrival
   *  so a stale plan from the prior thought doesn't linger. Returns
   *  the ids that were expired so the caller can notify. */
  expirePendingForChat(chatId: string, reason: string, byMsgId: string | null = null): string[] {
    const now = new Date().toISOString();
    const rows = this.db
      .prepare(
        `SELECT id FROM pending_plans
         WHERE chat_id = ? AND status = 'pending'`,
      )
      .all(chatId) as Array<{ id: string }>;
    if (rows.length === 0) return [];
    this.db
      .prepare(
        `UPDATE pending_plans
            SET status = 'expired', resolved_at = ?, resolution = ?,
                resolved_by_msg = ?, resolved_action = CASE WHEN ? IS NULL THEN NULL ELSE 'supersede' END
          WHERE chat_id = ? AND status = 'pending'`,
      )
      .run(now, reason, byMsgId, byMsgId, chatId);
    return rows.map((r) => r.id);
  }

  /** Sweep all `pending` rows older than `maxAgeMs` to `expired`. Returns
   *  the rows that were expired (so caller can notify per-chat). */
  sweepExpired(maxAgeMs: number): PendingPlanRow[] {
    const cutoff = new Date(Date.now() - maxAgeMs).toISOString();
    const rows = this.db
      .prepare(
        `SELECT * FROM pending_plans
         WHERE status = 'pending' AND captured_at < ?`,
      )
      .all(cutoff) as any[];
    if (rows.length === 0) return [];
    const now = new Date().toISOString();
    this.db
      .prepare(
        `UPDATE pending_plans
            SET status = 'expired', resolved_at = ?, resolution = 'idle timeout'
          WHERE status = 'pending' AND captured_at < ?`,
      )
      .run(now, cutoff);
    return rows.map(rowToPlan);
  }
}

function rowToPlan(row: any): PendingPlanRow {
  return {
    id: row.id,
    chatId: row.chat_id,
    capturedAt: row.captured_at,
    captureText: row.capture_text,
    inputKind: row.input_kind === "voice" ? "voice" : "text",
    opsJson: row.ops_json,
    summary: row.summary,
    confidence: row.confidence,
    status: row.status as PlanStatus,
    sourceMsgId: row.source_msg_id ?? null,
    resolvedByMsg: row.resolved_by_msg ?? null,
    resolvedAction: (row.resolved_action ?? null) as PlanAction | null,
    applyIds: row.apply_ids_json ? (JSON.parse(row.apply_ids_json) as number[]) : null,
    applyProgress: row.apply_progress_json ? JSON.parse(row.apply_progress_json) : {},
    outcomeJson: row.outcome_json ?? null,
  };
}

/** Short display id: first 4 hex chars of the UUID. */
export function shortPlanId(id: string): string {
  return id.replace(/-/g, "").slice(0, 4);
}
