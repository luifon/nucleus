// Durable record of conversational turns (ADR-033), in memory/whatsapp.db.
//
// chat_inbound — one row per operator message the chat engine accepted. The
//   `ref` is the marker typed with the message, which is how the engine finds
//   the message again in the session transcript. `text` is the complete
//   message (a retry replays it); `text_preview` is a short form for the
//   dashboard. (chat_id, wa_msg_id) is unique: a WhatsApp message delivered
//   twice is one row.
//     received → typed → consumed (a turn read it) → answered
//     … → failed | interrupted
// chat_turns — one row per session turn the engine observed.
//     running → done | silent | failed | interrupted
//   kind: operator (read at least one operator message), autonomous (the
//   session started it, e.g. a background command finished), context (an
//   attributed agent message), foreign (typed by something else, e.g. the
//   operator through `tmux attach`). quote_ref: the operator message the
//   turn's reply quotes. pending_bg: background commands still running when
//   the turn ended.
// seen_messages — WhatsApp message ids the bot started handling (inbound
//   dedup): received → handled. A message recorded as received but never
//   handled (the bot stopped in between) is handled again when WhatsApp
//   delivers it again; a handled one is dropped.
// task_scopes — sha256(scope token) → chat, for the tasks CLI (ADR-033).
// session_inbox — the queue other processes write (see db.ts); this store
//   only drains it.
//
// Tables are created by ChatSessionStore (db.ts); construct that first.

import { DatabaseSync } from "node:sqlite";
import { createHash } from "node:crypto";

export type InboundStatus =
  | "received"
  | "typed"
  | "consumed"
  | "answered"
  | "failed"
  | "interrupted";
export type TurnKind = "operator" | "autonomous" | "context" | "foreign";
export type TurnStatus = "running" | "done" | "silent" | "failed" | "interrupted";

export interface InboundRow {
  ref: string;
  chatId: string;
  pool: string;
  waMsgId: string | null;
  quotedJson: string | null;
  inputKind: string;
  /** The complete message text. */
  text: string;
  textPreview: string;
  receivedAt: string;
  turnId: string | null;
  status: InboundStatus;
  ackedAt: string | null;
  retried: number;
  /** `promptSource` of the transcript record the message arrived as
   *  ("typed"); null when unknown (queued while the session was busy). */
  promptSource: string | null;
  /** Typed chunks whose echo did not appear in time. */
  typingStalls: number;
}

export interface InboxRow {
  id: number;
  chat: string;
  sender: string;
  payload: string;
  attempts: number;
  enqueuedAt: string;
}

/** One restart note the boot sweep queued. */
export interface InterruptedItem {
  kind: "turn" | "message" | "background";
  turnId: string | null;
  chatId: string;
  quote: InboundRow | null;
  outboundId: number;
}

const PREVIEW_CHARS = 500;

function toInbound(r: any): InboundRow {
  return {
    ref: r.ref,
    chatId: r.chat_id,
    pool: r.pool,
    waMsgId: r.wa_msg_id,
    quotedJson: r.quoted_json,
    inputKind: r.input_kind,
    text: r.text ?? r.text_preview,
    textPreview: r.text_preview,
    receivedAt: r.received_at,
    turnId: r.turn_id,
    status: r.status,
    ackedAt: r.acked_at,
    retried: r.retried,
    promptSource: r.prompt_source ?? null,
    typingStalls: r.typing_stalls ?? 0,
  };
}

const now = () => new Date().toISOString();

export function sha256Hex(s: string): string {
  return createHash("sha256").update(s).digest("hex");
}

/** Inbound dedup for one bot process: the durable states of TurnStore plus
 *  the messages this process is handling right now, so a second delivery
 *  that arrives while the first is still being handled is dropped rather
 *  than handled twice at the same time. */
export class InboundGate {
  private inFlight = new Set<string>();
  constructor(private readonly store: Pick<TurnStore, "beginInbound" | "markHandled">) {}

  /** True when the caller must handle the message; false: drop it. */
  begin(chatId: string, waMsgId: string): { handle: boolean; retry: boolean } {
    const key = `${chatId}\u0000${waMsgId}`;
    if (this.inFlight.has(key)) return { handle: false, retry: false };
    const state = this.store.beginInbound(chatId, waMsgId);
    if (state === "handled") return { handle: false, retry: false };
    this.inFlight.add(key);
    return { handle: true, retry: state === "retry" };
  }

  /** The hand-off completed (`ok`), or failed and may be handled again. */
  end(chatId: string, waMsgId: string, ok: boolean): void {
    this.inFlight.delete(`${chatId}\u0000${waMsgId}`);
    if (ok) this.store.markHandled(chatId, waMsgId);
  }
}

export class TurnStore {
  private db: DatabaseSync;

  constructor(dbPath: string) {
    this.db = new DatabaseSync(dbPath);
    this.db.exec(`PRAGMA journal_mode = WAL;`);
    this.db.exec(`PRAGMA busy_timeout = 5000;`);
  }

  /** Run `fn` in one BEGIN IMMEDIATE transaction. */
  private tx<T>(fn: () => T): T {
    this.db.exec("BEGIN IMMEDIATE");
    try {
      const out = fn();
      this.db.exec("COMMIT");
      return out;
    } catch (e) {
      this.db.exec("ROLLBACK");
      throw e;
    }
  }

  // ── inbound dedup ──

  /** Record that the bot starts handling WhatsApp message `waMsgId` of
   *  `chatId`. `new`: first delivery. `retry`: an earlier delivery was
   *  received but its hand-off never completed (the bot stopped); handle it
   *  again. `handled`: drop it. */
  beginInbound(chatId: string, waMsgId: string): "new" | "retry" | "handled" {
    const res = this.db
      .prepare(`INSERT OR IGNORE INTO seen_messages (chat_id, wa_msg_id, seen_at, status) VALUES (?, ?, ?, 'received')`)
      .run(chatId, waMsgId, now());
    if (Number(res.changes) === 1) return "new";
    const row = this.db
      .prepare(`SELECT status FROM seen_messages WHERE chat_id = ? AND wa_msg_id = ?`)
      .get(chatId, waMsgId) as { status: string } | undefined;
    return row?.status === "received" ? "retry" : "handled";
  }

  /** The message's durable hand-off completed: a later delivery is dropped. */
  markHandled(chatId: string, waMsgId: string): void {
    this.db
      .prepare(`UPDATE seen_messages SET status = 'handled' WHERE chat_id = ? AND wa_msg_id = ?`)
      .run(chatId, waMsgId);
  }

  /** Drop dedup entries older than `maxAgeMs` (Baileys replays are recent). */
  pruneSeen(maxAgeMs: number): void {
    this.db
      .prepare(`DELETE FROM seen_messages WHERE seen_at < ?`)
      .run(new Date(Date.now() - maxAgeMs).toISOString());
  }

  // ── inbound ──

  /** Insert an operator message. When a row for the same (chat, WhatsApp
   *  message id) exists, nothing is inserted and that row's ref comes back
   *  with `duplicate: true`. */
  addInbound(i: {
    ref: string;
    chatId: string;
    pool: string;
    waMsgId: string | null;
    quotedJson: string | null;
    inputKind: string;
    text: string;
  }): { ref: string; duplicate: boolean } {
    const res = this.db
      .prepare(
        `INSERT OR IGNORE INTO chat_inbound
           (ref, chat_id, pool, wa_msg_id, quoted_json, input_kind, text, text_preview,
            received_at, status)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'received')`,
      )
      .run(
        i.ref,
        i.chatId,
        i.pool,
        i.waMsgId,
        i.quotedJson,
        i.inputKind,
        i.text,
        Array.from(i.text).slice(0, PREVIEW_CHARS).join(""),
        now(),
      );
    if (Number(res.changes) === 1) return { ref: i.ref, duplicate: false };
    const existing = this.db
      .prepare(`SELECT ref FROM chat_inbound WHERE chat_id = ? AND wa_msg_id = ?`)
      .get(i.chatId, i.waMsgId) as { ref: string } | undefined;
    if (!existing) throw new Error(`chat_inbound insert ignored for ref ${i.ref}`);
    return { ref: existing.ref, duplicate: true };
  }

  getInbound(ref: string): InboundRow | null {
    const r = this.db.prepare(`SELECT * FROM chat_inbound WHERE ref = ?`).get(ref);
    return r ? toInbound(r) : null;
  }

  /** The message was typed and submitted. `submit` is what the submit
   *  observed: the prompt's `promptSource` and the typing stalls. */
  markTyped(ref: string, submit?: { promptSource: string | null; typingStalls: number }): void {
    this.db
      .prepare(
        `UPDATE chat_inbound SET typed_at = ?, status = 'typed', prompt_source = ?, typing_stalls = ?
          WHERE ref = ? AND status = 'received'`,
      )
      .run(now(), submit?.promptSource ?? null, submit?.typingStalls ?? 0, ref);
  }

  /** A turn read this message. A message that arrived as pasted content
   *  marks the turn (chat_turns.pasted_input). */
  markConsumed(ref: string, turnId: string): void {
    this.db
      .prepare(
        `UPDATE chat_inbound SET turn_id = ?, status = 'consumed'
          WHERE ref = ? AND status IN ('received','typed','consumed')`,
      )
      .run(turnId, ref);
    this.db
      .prepare(
        `UPDATE chat_turns SET pasted_input = 1
          WHERE id = ? AND EXISTS (SELECT 1 FROM chat_inbound
                                    WHERE ref = ? AND prompt_source IS NOT NULL AND prompt_source != 'typed')`,
      )
      .run(turnId, ref);
  }

  markAnswered(refs: string[]): void {
    const st = this.db.prepare(
      `UPDATE chat_inbound SET status = 'answered', answered_at = ?
        WHERE ref = ? AND status IN ('received','typed','consumed')`,
    );
    for (const r of refs) st.run(now(), r);
  }

  markInboundFailed(ref: string, error: string): void {
    this.db
      .prepare(
        `UPDATE chat_inbound SET status = 'failed', error = ?
          WHERE ref = ? AND status IN ('received','typed','consumed')`,
      )
      .run(error.slice(0, 1000), ref);
  }

  markAcked(ref: string): void {
    this.db.prepare(`UPDATE chat_inbound SET acked_at = ? WHERE ref = ?`).run(now(), ref);
  }

  markRetried(ref: string): void {
    this.db
      .prepare(`UPDATE chat_inbound SET retried = retried + 1, status = 'received', turn_id = NULL WHERE ref = ?`)
      .run(ref);
  }

  /** Operator messages of `chatId` that have no reply yet, oldest first. */
  unanswered(chatId: string): InboundRow[] {
    return (
      this.db
        .prepare(
          `SELECT * FROM chat_inbound
            WHERE chat_id = ? AND status IN ('received','typed','consumed')
            ORDER BY received_at, rowid`,
        )
        .all(chatId) as any[]
    ).map(toInbound);
  }

  // ── turns ──

  startTurn(t: {
    id: string;
    chatId: string;
    pool: string;
    sessionId: string;
    kind: TurnKind;
    quoteRef?: string | null;
  }): void {
    this.db
      .prepare(
        `INSERT INTO chat_turns (id, chat_id, pool, session_id, kind, status, started_at, quote_ref)
         VALUES (?, ?, ?, ?, ?, 'running', ?, ?)`,
      )
      .run(t.id, t.chatId, t.pool, t.sessionId, t.kind, now(), t.quoteRef ?? null);
  }

  setTurnKind(id: string, kind: TurnKind): void {
    this.db.prepare(`UPDATE chat_turns SET kind = ? WHERE id = ?`).run(kind, id);
  }

  setTurnQuote(id: string, quoteRef: string): void {
    this.db.prepare(`UPDATE chat_turns SET quote_ref = COALESCE(quote_ref, ?) WHERE id = ?`).run(quoteRef, id);
  }

  markTurnAck(id: string): void {
    this.db.prepare(`UPDATE chat_turns SET ack_sent = 1 WHERE id = ?`).run(id);
  }

  bumpProgress(id: string): void {
    this.db.prepare(`UPDATE chat_turns SET progress_count = progress_count + 1 WHERE id = ?`).run(id);
  }

  endTurn(
    id: string,
    e: {
      status: TurnStatus;
      finalOutboundId?: number | null;
      replyChars?: number | null;
      error?: string | null;
      pendingBg?: number | null;
    },
  ): void {
    this.db
      .prepare(
        `UPDATE chat_turns
            SET status = ?, ended_at = ?, final_outbound_id = COALESCE(?, final_outbound_id),
                reply_chars = COALESCE(?, reply_chars), error = COALESCE(?, error),
                pending_bg = COALESCE(?, pending_bg)
          WHERE id = ? AND status = 'running'`,
      )
      .run(
        e.status,
        now(),
        e.finalOutboundId ?? null,
        e.replyChars ?? null,
        e.error ?? null,
        e.pendingBg ?? null,
        id,
      );
  }

  /** The background commands of `chatId`'s turns finished (a later turn
   *  answered them): nothing is owed any more. */
  clearPendingBackground(chatId: string): void {
    this.db.prepare(`UPDATE chat_turns SET pending_bg = 0 WHERE chat_id = ? AND pending_bg > 0`).run(chatId);
  }

  inboundOfTurn(turnId: string): InboundRow[] {
    return (
      this.db
        .prepare(`SELECT * FROM chat_inbound WHERE turn_id = ? ORDER BY received_at, rowid`)
        .all(turnId) as any[]
    ).map(toInbound);
  }

  /** Boot sweep (ADR-033 restart handling). The previous process owned
   *  every running turn, unanswered message and running background command,
   *  and the boot wipe kills their sessions, so each is interrupted. One
   *  note is queued per running turn that owes a reply (an operator turn
   *  with open messages, or an autonomous turn answering a background
   *  command), per message no turn had read yet, and per chat whose last
   *  turn left background commands running. The state changes and the
   *  outbound rows are one transaction, and every note has a unique
   *  delivery key, so a crash can neither lose a note nor send it twice.
   *  No automatic resume. */
  sweepInterrupted(notes: { interrupted: string; backgroundLost: string }, format: (s: string) => string): InterruptedItem[] {
    return this.tx(() => {
      const items: InterruptedItem[] = [];
      const enqueue = (chatId: string, body: string, quote: InboundRow | null, key: string): number => {
        this.db
          .prepare(
            `INSERT OR IGNORE INTO outbound_queue
               (target, body, source, enqueued_at, status, attempts, kind, quoted_json, dedup_key)
             VALUES (?, ?, 'chat-interrupted', ?, 'pending', 0, 'text', ?, ?)`,
          )
          .run(chatId, format(body), now(), quote?.quotedJson ?? null, key);
        const row = this.db.prepare(`SELECT id FROM outbound_queue WHERE dedup_key = ?`).get(key) as {
          id: number;
        };
        return row.id;
      };

      const turns = this.db
        .prepare(`SELECT id, chat_id, kind, quote_ref FROM chat_turns WHERE status = 'running'`)
        .all() as Array<{ id: string; chat_id: string; kind: string; quote_ref: string | null }>;
      const covered = new Set<string>();
      for (const t of turns) {
        const open = this.inboundOfTurn(t.id).filter((i) => ["received", "typed", "consumed"].includes(i.status));
        open.forEach((i) => covered.add(i.ref));
        let quote: InboundRow | null = open[0] ?? null;
        if (!quote && t.kind === "autonomous" && t.quote_ref) quote = this.getInbound(t.quote_ref);
        const owesReply = open.length > 0 || (t.kind === "autonomous" && quote !== null);
        // A context or foreign turn owes nobody a reply: no note.
        if (!owesReply) continue;
        const id = enqueue(t.chat_id, notes.interrupted, quote, `interrupted:turn:${t.id}`);
        items.push({ kind: "turn", turnId: t.id, chatId: t.chat_id, quote, outboundId: id });
      }
      const loose = (
        this.db.prepare(`SELECT * FROM chat_inbound WHERE status IN ('received','typed','consumed')`).all() as any[]
      )
        .map(toInbound)
        .filter((i) => !covered.has(i.ref));
      for (const i of loose) {
        const id = enqueue(i.chatId, notes.interrupted, i, `interrupted:message:${i.ref}`);
        items.push({ kind: "message", turnId: null, chatId: i.chatId, quote: i, outboundId: id });
      }
      // A turn that ended while background commands ran promised a later
      // result; the restart killed those commands with the session.
      const waiting = this.db
        .prepare(`SELECT id, chat_id, quote_ref FROM chat_turns WHERE pending_bg > 0 AND status != 'running'`)
        .all() as Array<{ id: string; chat_id: string; quote_ref: string | null }>;
      for (const t of waiting) {
        const quote = t.quote_ref ? this.getInbound(t.quote_ref) : null;
        const id = enqueue(t.chat_id, notes.backgroundLost, quote, `interrupted:background:${t.id}`);
        items.push({ kind: "background", turnId: t.id, chatId: t.chat_id, quote, outboundId: id });
      }

      const ts = now();
      this.db
        .prepare(
          `UPDATE chat_turns SET status = 'interrupted', ended_at = ?, error = 'bot restarted during the turn'
            WHERE status = 'running'`,
        )
        .run(ts);
      this.db.prepare(`UPDATE chat_turns SET pending_bg = 0 WHERE pending_bg > 0`).run();
      this.db
        .prepare(
          `UPDATE chat_inbound SET status = 'interrupted', error = 'bot restarted before the reply'
            WHERE status IN ('received','typed','consumed')`,
        )
        .run();
      return items;
    });
  }

  // ── task scopes ──

  /** Make `token` the only valid task scope of `chatId` (one active
   *  session per chat): earlier tokens of the chat are revoked in the same
   *  transaction. */
  setTaskScope(chatId: string, token: string): void {
    this.tx(() => {
      this.db.prepare(`DELETE FROM task_scopes WHERE chat_id = ?`).run(chatId);
      this.db
        .prepare(`INSERT INTO task_scopes (token_sha256, chat_id, created_at) VALUES (?, ?, ?)`)
        .run(sha256Hex(token), chatId, now());
    });
  }

  /** Revoke every task scope of `chatId` (its session closed or failed). */
  revokeTaskScopes(chatId: string): void {
    this.db.prepare(`DELETE FROM task_scopes WHERE chat_id = ?`).run(chatId);
  }

  /** True when `token` is a valid task scope. */
  taskScopeValid(token: string): boolean {
    return this.db.prepare(`SELECT 1 FROM task_scopes WHERE token_sha256 = ?`).get(sha256Hex(token)) !== undefined;
  }

  /** Boot: every chat session is respawned with a new token. */
  clearTaskScopes(): void {
    this.db.prepare(`DELETE FROM task_scopes`).run();
  }

  // ── session_inbox ──

  pendingInbox(limit = 10): InboxRow[] {
    return (
      this.db
        .prepare(
          `SELECT id, chat, sender, payload, attempts, enqueued_at FROM session_inbox
            WHERE status = 'pending' ORDER BY id LIMIT ?`,
        )
        .all(limit) as any[]
    ).map((r) => ({
      id: r.id,
      chat: r.chat,
      sender: r.sender,
      payload: r.payload,
      attempts: r.attempts,
      enqueuedAt: r.enqueued_at,
    }));
  }

  markInboxDelivered(id: number): void {
    this.db
      .prepare(`UPDATE session_inbox SET status = 'delivered', delivered_at = ? WHERE id = ?`)
      .run(now(), id);
  }

  /** Taken by the engine (typing in progress); not re-read by the drain. */
  markInboxClaimed(id: number): void {
    this.db.prepare(`UPDATE session_inbox SET status = 'claimed' WHERE id = ? AND status = 'pending'`).run(id);
  }

  markInboxFailure(id: number, error: string, maxAttempts: number): void {
    const row = this.db.prepare(`SELECT attempts FROM session_inbox WHERE id = ?`).get(id) as
      | { attempts: number }
      | undefined;
    const attempts = (row?.attempts ?? 0) + 1;
    this.db
      .prepare(`UPDATE session_inbox SET attempts = ?, last_error = ?, status = ? WHERE id = ?`)
      .run(attempts, error.slice(0, 1000), attempts >= maxAttempts ? "failed" : "pending", id);
  }

  /** Boot: rows a dead process had claimed go back to pending. */
  releaseClaimedInbox(): void {
    this.db.prepare(`UPDATE session_inbox SET status = 'pending' WHERE status = 'claimed'`).run();
  }
}
