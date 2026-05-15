// Uses node:sqlite (Node 22.5+, stable in Node 24). No native build needed.
import { DatabaseSync } from "node:sqlite";
import { randomUUID } from "node:crypto";
import fs from "node:fs";
import path from "node:path";

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
        msg_id TEXT
      );

      CREATE INDEX IF NOT EXISTS idx_outbound_status_enqueued
        ON outbound_queue(status, enqueued_at);
    `);
  }

  lookup(chatId: string): string | null {
    const row = this.db
      .prepare("SELECT session_id FROM chat_sessions WHERE chat_id = ?")
      .get(chatId) as { session_id: string } | undefined;
    return row?.session_id ?? null;
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
  /** Display label shown to user, e.g. "2-Areas/Nucleus". Also serves as
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

export interface OutboundRow {
  id: number;
  target: string;
  body: string;
  source: string;
  enqueuedAt: string;
  attempts: number;
}

/** Outbound WhatsApp send queue. The reminders binary (and anyone else
 *  who needs to send a WhatsApp message from outside Alfred's process)
 *  inserts rows here. Alfred's main process drains every 5s, resolves
 *  `target` to a JID via the allowlist, and sends via Baileys.
 *
 *  `target` is either a group name ("Alfred", "Brain Dump") OR a raw
 *  JID. Alfred's drainer accepts both — but only if the resolved JID
 *  is on the allowlist (no sending to arbitrary chats).
 *
 *  Failures bump `attempts`; after a max-attempts threshold, status
 *  moves to 'failed' to stop retry storms. */
export class OutboundQueueStore {
  private db: DatabaseSync;

  constructor(dbPath: string) {
    this.db = new DatabaseSync(dbPath);
    this.db.exec(`PRAGMA journal_mode = WAL;`);
  }

  /** Up-to-`limit` pending rows, oldest first. */
  pending(limit: number = 20): OutboundRow[] {
    const rows = this.db
      .prepare(
        `SELECT id, target, body, source, enqueued_at, attempts
           FROM outbound_queue
          WHERE status = 'pending'
          ORDER BY enqueued_at ASC
          LIMIT ?`,
      )
      .all(limit) as Array<{
        id: number;
        target: string;
        body: string;
        source: string;
        enqueued_at: string;
        attempts: number;
      }>;
    return rows.map((r) => ({
      id: r.id,
      target: r.target,
      body: r.body,
      source: r.source,
      enqueuedAt: r.enqueued_at,
      attempts: r.attempts,
    }));
  }

  markSent(id: number, msgId: string): void {
    const now = new Date().toISOString();
    this.db
      .prepare(
        `UPDATE outbound_queue
            SET status = 'sent', sent_at = ?, msg_id = ?
          WHERE id = ?`,
      )
      .run(now, msgId, id);
  }

  /** Record a delivery failure. After `maxAttempts` we stop retrying. */
  markFailure(id: number, error: string, maxAttempts: number): void {
    const row = this.db
      .prepare(
        `SELECT attempts FROM outbound_queue WHERE id = ?`,
      )
      .get(id) as { attempts: number } | undefined;
    const attempts = (row?.attempts ?? 0) + 1;
    const status = attempts >= maxAttempts ? "failed" : "pending";
    this.db
      .prepare(
        `UPDATE outbound_queue
            SET attempts = ?, last_error = ?, status = ?
          WHERE id = ?`,
      )
      .run(attempts, error, status, id);
  }
}
