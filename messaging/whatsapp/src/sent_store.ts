// The content of every message this device sent, by WhatsApp message id
// (ADR-027 amendment, 2026-09).
//
// When a recipient device cannot decrypt a message (its Signal session with
// this device changed, for example after a reconnect), it sends a retry
// receipt. Baileys answers the receipt by encrypting the message again, and
// it gets the content from the socket's `getMessage` callback. Without it the
// retry fails and the recipient shows "waiting for this message". The
// per-socket cache Baileys keeps is lost on every reconnect, so the bot
// stores the content here, in whatsapp.db.
//
// Retention: WhatsApp accepts resend requests for messages up to
// PLACEHOLDER_MAX_AGE_SECONDS old (14 days in Baileys 7.0.0-rc14), so the
// content is kept for SENT_RETENTION_MS (21 days) by default, and never for
// less than that upstream window. `[whatsapp.link] sent_retention_days`
// changes it. The table is also capped at `sent_max_rows` rows (oldest
// deleted first) so a burst of sends cannot grow it without limit.
//
// Schema: `sent_messages`, created by ChatSessionStore (db.ts).

import { DatabaseSync } from "node:sqlite";
import { PLACEHOLDER_MAX_AGE_SECONDS, proto, type WAMessage } from "@whiskeysockets/baileys";

const DAY_MS = 24 * 60 * 60 * 1000;
export const SENT_RETENTION_MS = 21 * DAY_MS;
export const SENT_MAX_ROWS = 50_000;
/** The shortest retention allowed: the upstream resend window. */
export const SENT_MIN_RETENTION_MS = PLACEHOLDER_MAX_AGE_SECONDS * 1000;

export interface SentStoreOptions {
  retentionMs?: number;
  maxRows?: number;
}

export class SentMessageStore {
  private db: DatabaseSync;
  readonly retentionMs: number;
  readonly maxRows: number;

  constructor(dbPath: string, opts: SentStoreOptions = {}) {
    this.db = new DatabaseSync(dbPath);
    this.db.exec(`PRAGMA journal_mode = WAL;`);
    this.retentionMs = Math.max(opts.retentionMs ?? SENT_RETENTION_MS, SENT_MIN_RETENTION_MS);
    this.maxRows = Math.max(1, Math.floor(opts.maxRows ?? SENT_MAX_ROWS));
  }

  /** Store a sent message. A message without an id, a chat or content is
   *  skipped. Storing the same id again keeps the first row. */
  record(msg: WAMessage | undefined, nowMs = Date.now()): boolean {
    const id = msg?.key?.id;
    const jid = msg?.key?.remoteJid;
    if (!id || !jid || !msg?.message) return false;
    const bytes = proto.Message.encode(msg.message).finish();
    const res = this.db
      .prepare(`INSERT OR IGNORE INTO sent_messages (id, jid, proto, sent_at) VALUES (?, ?, ?, ?)`)
      .run(id, jid, bytes, new Date(nowMs).toISOString());
    return Number(res.changes) > 0;
  }

  /** The content of sent message `id`, or undefined when it is not stored
   *  (Baileys then leaves the retry unanswered). */
  get(id: string | null | undefined): proto.IMessage | undefined {
    if (!id) return undefined;
    const row = this.db.prepare(`SELECT proto FROM sent_messages WHERE id = ?`).get(id) as
      | { proto: Uint8Array }
      | undefined;
    if (!row) return undefined;
    try {
      return proto.Message.decode(row.proto);
    } catch {
      return undefined;
    }
  }

  /** Delete messages older than the retention, then the oldest rows beyond
   *  the row cap — but the cap never deletes a message younger than the
   *  upstream resend window (SENT_MIN_RETENTION_MS), so a resend request
   *  inside that window is always answerable. Returns how many rows were
   *  deleted. */
  prune(nowMs = Date.now()): number {
    const cutoff = new Date(nowMs - this.retentionMs).toISOString();
    const aged = this.db.prepare(`DELETE FROM sent_messages WHERE sent_at < ?`).run(cutoff);
    const protectedFrom = new Date(nowMs - SENT_MIN_RETENTION_MS).toISOString();
    const capped = this.db
      .prepare(
        `DELETE FROM sent_messages WHERE sent_at < ? AND id IN (
           SELECT id FROM sent_messages ORDER BY sent_at DESC, id DESC LIMIT -1 OFFSET ?
         )`,
      )
      .run(protectedFrom, this.maxRows);
    return Number(aged.changes) + Number(capped.changes);
  }
}
