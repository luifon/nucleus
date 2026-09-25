// The outbound queue drain (ADR-018 / ADR-020 / ADR-033), separated from
// index.ts so its send protocol can be tested without a socket.
//
// For each row, oldest first:
//   1. Resolve the target to an allowlisted JID (the caller's resolver);
//      refuse anything else.
//   2. Build the content; pass the text through the secret filter
//      (secret_filter.ts). A progress message that matches is withheld.
//   3. Validate the stored quote against the resolved chat.
//   4. Mark the row in_flight with its WhatsApp message id, then send with
//      that id. Success → sent. A rejection → pending (retried with the same
//      id). A timeout → the row stays in_flight while the send can still
//      succeed: a late success marks it sent, and only after
//      IN_FLIGHT_GRACE_MS, with no send of it pending in this process, is it
//      attempted again — with the same id. A server acknowledgement for that
//      id (`onServerAck`, also after a reconnect) marks the row sent.
//
// Re-entrancy: one tick at a time. Connection rot: consecutive
// "Connection Closed"-shaped failures or hangs past a threshold call
// `fatal` (the process exits for a launchd respawn).

import { BufferJSON, type AnyMessageContent, type WAMessage } from "@whiskeysockets/baileys";
import type { OutboundQueueStore, OutboundRow } from "./db.js";
import {
  buildOutboundContent,
  cleanupMedia,
  MAX_MEDIA_SENDS_PER_TICK,
  sendTimeoutFor,
} from "./outbound.js";
import { filterOutbound, type Audience, type SecretRules } from "./secret_filter.js";

export const OUTBOUND_MAX_ATTEMPTS = 5;
export const CONNECTION_ROT_THRESHOLD = 5;

export class SendTimeoutError extends Error {
  constructor(ms: number) {
    super(`sendMessage timed out after ${ms}ms (may still deliver late)`);
    this.name = "SendTimeoutError";
  }
}

export function withTimeout<T>(p: Promise<T>, ms: number): Promise<T> {
  let timer: NodeJS.Timeout;
  return Promise.race([
    p,
    new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new SendTimeoutError(ms)), ms);
    }),
  ]).finally(() => clearTimeout(timer!));
}

/** The stored message a queued reply quotes, if it is a complete message of
 *  the chat the row is sent to. A quote that does not decode, lacks a key
 *  id, or belongs to another chat is dropped (the row is sent unquoted), so
 *  a queue writer cannot attach an arbitrary message to a reply. Pure. */
export function parseQuoted(json: string | null, jid: string): WAMessage | undefined {
  if (!json) return undefined;
  let q: any;
  try {
    q = JSON.parse(json, BufferJSON.reviver);
  } catch {
    return undefined;
  }
  const key = q?.key;
  if (!key || typeof key.id !== "string" || !key.id || typeof key.remoteJid !== "string") return undefined;
  if (key.remoteJid !== jid) return undefined;
  if (!q.message || typeof q.message !== "object") return undefined;
  return q as WAMessage;
}

export interface DrainLog {
  info(obj: object, msg: string): void;
  warn(obj: object, msg: string): void;
  error(obj: object, msg: string): void;
}

export interface DrainDeps {
  store: OutboundQueueStore;
  /** Allowlisted JID for a row target, or null. */
  resolveTarget: (target: string) => string | null;
  send: (jid: string, content: AnyMessageContent, opts: { messageId: string; quoted?: WAMessage }) => Promise<WAMessage | undefined>;
  newMessageId: () => string;
  rules: () => SecretRules;
  /** The note appended to a redacted message ({count}). */
  withheldNote: string;
  mediaMaxBytes: number;
  log: DrainLog;
  /** Unrecoverable socket state: alert and exit. */
  fatal: (msg: string) => Promise<void>;
  timeoutFor?: (kind: OutboundRow["kind"]) => number;
}

export class OutboundDrain {
  private draining = false;
  private tickStartedAt: number | null = null;
  /** Rows with a send promise that has not settled, in this process. */
  private readonly active = new Set<number>();
  private consecutiveFailures = 0;

  constructor(private readonly d: DrainDeps) {}

  /** ms the current tick has run, or null when idle (the watchdog). */
  runningFor(): number | null {
    return this.draining && this.tickStartedAt !== null ? Date.now() - this.tickStartedAt : null;
  }

  /** A server acknowledgement for one of our message ids. */
  onServerAck(msgId: string): void {
    const id = this.d.store.markSentByMsgId(msgId);
    if (id !== null) this.d.log.info({ id, msgId }, "whatsapp: outbound acknowledged by the server — marked sent");
  }

  async tick(): Promise<void> {
    if (this.draining) return;
    this.draining = true;
    this.tickStartedAt = Date.now();
    try {
      await this.drain();
    } finally {
      this.draining = false;
      this.tickStartedAt = null;
    }
  }

  private async drain(): Promise<void> {
    let rows: OutboundRow[];
    try {
      rows = this.d.store.pending(20, this.active);
    } catch (e) {
      this.d.log.warn({ err: (e as Error).message }, "whatsapp: outbound pending() failed");
      return;
    }
    if (rows.length === 0) return;
    this.d.log.info({ count: rows.length }, "whatsapp: draining outbound queue");
    let mediaSent = 0;
    for (const r of rows) {
      if (r.kind !== "text" && mediaSent >= MAX_MEDIA_SENDS_PER_TICK) continue;
      const jid = this.d.resolveTarget(r.target);
      if (!jid) {
        const { status } = this.d.store.markFailure(r.id, `unknown target: ${r.target}`, OUTBOUND_MAX_ATTEMPTS);
        if (status === "failed") cleanupMedia(r);
        this.d.log.warn({ id: r.id, target: r.target }, "whatsapp: outbound target not in allowlist — failed");
        continue;
      }
      const content = buildOutboundContent(r, this.d.mediaMaxBytes);
      if ("error" in content) {
        this.d.store.markFailedTerminal(r.id, content.error);
        cleanupMedia(r);
        this.d.log.warn({ id: r.id, err: content.error }, "whatsapp: outbound media row terminal-failed");
        continue;
      }
      const filtered = this.applySecretFilter(r, jid, content);
      if (filtered === null) continue;
      const stop = await this.sendRow(r, jid, filtered);
      if (r.kind !== "text") mediaSent += 1;
      if (this.consecutiveFailures >= CONNECTION_ROT_THRESHOLD) {
        await this.d.fatal(
          `⚠️ WhatsApp bot exiting: ${this.consecutiveFailures} consecutive failed/hung sendMessage calls — launchd will respawn.`,
        );
        return;
      }
      if (stop) break;
    }
  }

  /** The content with its text filtered, or null when the row was withheld. */
  private applySecretFilter(r: OutboundRow, jid: string, content: AnyMessageContent): AnyMessageContent | null {
    const audience: Audience = jid.endsWith("@g.us") ? "shared" : "operator-dm";
    const progress = r.source === "chat-progress";
    const c = content as Record<string, any>;
    const field = typeof c.text === "string" ? "text" : typeof c.caption === "string" ? "caption" : null;
    if (field === null) return content;
    const res = filterOutbound(c[field], this.d.rules(), { audience, progress, note: this.d.withheldNote });
    if (res.hits.length === 0) return content;
    const kinds = [...new Set(res.hits)];
    if (res.withheld) {
      this.d.store.markFailedTerminal(r.id, `withheld by the secret filter (${kinds.join(", ")})`);
      cleanupMedia(r);
      this.d.log.warn({ id: r.id, source: r.source, hits: res.hits.length, kinds }, "whatsapp: outbound withheld by the secret filter");
      return null;
    }
    this.d.log.warn({ id: r.id, source: r.source, hits: res.hits.length, kinds }, "whatsapp: outbound values redacted by the secret filter");
    return { ...c, [field]: res.text } as AnyMessageContent;
  }

  /** Returns true when the tick should stop (a hung socket). */
  private async sendRow(r: OutboundRow, jid: string, content: AnyMessageContent): Promise<boolean> {
    const msgId = this.d.store.markInFlight(r.id, r.msgId ?? this.d.newMessageId());
    const quoted = parseQuoted(r.quotedJson, jid);
    let promise: Promise<WAMessage | undefined>;
    try {
      promise = this.d.send(jid, content, quoted ? { messageId: msgId, quoted } : { messageId: msgId });
    } catch (e) {
      promise = Promise.reject(e);
    }
    this.active.add(r.id);
    const settled = promise.then(
      (sent) => {
        this.active.delete(r.id);
        return { ok: true as const, sent };
      },
      (err: Error) => {
        this.active.delete(r.id);
        return { ok: false as const, err };
      },
    );
    const timeoutMs = (this.d.timeoutFor ?? sendTimeoutFor)(r.kind);
    let outcome: Awaited<typeof settled>;
    try {
      outcome = await withTimeout(settled, timeoutMs);
    } catch (e) {
      if (!(e instanceof SendTimeoutError)) throw e;
      // The send may still succeed: keep the row in_flight (not retryable
      // until the grace period passes with this promise settled), and let a
      // late result decide.
      const { status } = this.d.store.markTimedOut(r.id, e.message, OUTBOUND_MAX_ATTEMPTS);
      this.consecutiveFailures += 1;
      void settled.then((late) => {
        if (late.ok) {
          this.d.store.markSent(r.id, late.sent?.key?.id ?? msgId);
          cleanupMedia(r);
          this.d.log.warn({ id: r.id }, "whatsapp: timed-out send completed late — marked sent");
        } else if (this.d.store.status(r.id) === "in_flight") {
          const { status: s } = this.d.store.markFailure(r.id, late.err.message, OUTBOUND_MAX_ATTEMPTS);
          if (s === "failed") cleanupMedia(r);
        }
      });
      if (status === "failed") cleanupMedia(r);
      this.d.log.warn(
        { id: r.id, kind: r.kind, jid, consecutive: this.consecutiveFailures },
        "whatsapp: outbound send timed out — aborting tick (hung socket won't recover row-to-row)",
      );
      return true;
    }
    if (outcome.ok) {
      this.d.store.markSent(r.id, outcome.sent?.key?.id ?? msgId);
      cleanupMedia(r);
      this.consecutiveFailures = 0;
      this.d.log.info({ id: r.id, kind: r.kind, target: r.target, jid }, "whatsapp: outbound sent");
      return false;
    }
    const err = outcome.err.message;
    const { status } = this.d.store.markFailure(r.id, err, OUTBOUND_MAX_ATTEMPTS);
    if (status === "failed") cleanupMedia(r);
    this.d.log.warn({ id: r.id, err, attempts: r.attempts + 1 }, "whatsapp: outbound send failed");
    if (/connection closed/i.test(err)) {
      this.consecutiveFailures += 1;
      this.d.log.warn(
        { consecutive: this.consecutiveFailures, threshold: CONNECTION_ROT_THRESHOLD },
        "whatsapp: connection-rot counter incremented",
      );
    }
    return false;
  }
}
