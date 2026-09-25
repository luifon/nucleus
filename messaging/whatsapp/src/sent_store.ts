// The content of every message this device sent, by WhatsApp message id
// (ADR-027 amendment, 2026-09).
//
// When a recipient device cannot decrypt a message (its Signal session with
// this device changed, for example after a reconnect), it sends a retry
// receipt. Baileys answers the receipt by encrypting the message again, and
// it gets the content from the socket's `getMessage` callback. Without it the
// retry fails and the recipient shows "waiting for this message". The
// per-socket cache Baileys keeps is lost on every reconnect, so the bot
// stores the content here, in whatsapp.db, for SENT_RETENTION_MS.
//
// Schema: `sent_messages`, created by ChatSessionStore (db.ts).

import { DatabaseSync } from "node:sqlite";
import { proto, type WAMessage } from "@whiskeysockets/baileys";

export const SENT_RETENTION_MS = 7 * 24 * 60 * 60 * 1000;

export class SentMessageStore {
  private db: DatabaseSync;

  constructor(dbPath: string) {
    this.db = new DatabaseSync(dbPath);
    this.db.exec(`PRAGMA journal_mode = WAL;`);
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

  /** Delete messages older than `retentionMs`. Returns how many. */
  prune(retentionMs = SENT_RETENTION_MS, nowMs = Date.now()): number {
    const cutoff = new Date(nowMs - retentionMs).toISOString();
    const res = this.db.prepare(`DELETE FROM sent_messages WHERE sent_at < ?`).run(cutoff);
    return Number(res.changes);
  }
}
