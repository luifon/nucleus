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
// Re-entrancy: one tick at a time.
//
// Link gating (ADR-027 amendment, 2026-09): the drain sends only while the
// WhatsApp link is open (`linkUp` on connection open, `linkDown` on close).
// A send that fails because the link dropped (a link-shaped error, or the
// link closed while the send was pending) puts the row back to pending with
// the same message id and does not count toward the row's attempts: an
// outage does not use up a message's retries. Connection rot: consecutive
// link-shaped failures or hangs while the link reports open, at least
// CONNECTION_ROT_THRESHOLD of them over at least ROT_MIN_SPAN_MS, call
// `fatal` (the process exits for a launchd respawn). A link close resets the
// count, so the failures that precede a normal close never cause an exit.

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
/** The rot exit also needs the failure streak to last this long: a close
 *  event can arrive a moment after the sends it broke have failed. */
export const ROT_MIN_SPAN_MS = 30_000;

/** A send error caused by the link, not by the message: the socket was
 *  closed or lost, or a query timed out waiting for the server. Boom errors
 *  carry the status in `output.statusCode` (Baileys DisconnectReason: 408
 *  connectionLost/timedOut, 428 connectionClosed, 503 unavailableService,
 *  515 restartRequired). */
export function isLinkError(err: unknown): boolean {
  const code = (err as { output?: { statusCode?: number } } | null)?.output?.statusCode;
  if (code === 408 || code === 428 || code === 503 || code === 515) return true;
  const msg = err instanceof Error ? err.message : String(err);
  return /connection (closed|lost|terminated)|no live socket|stream errored|socket (closed|hang up)|websocket (is )?not open/i.test(msg);
}

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
  /** Override of ROT_MIN_SPAN_MS (tests). */
  rotMinSpanMs?: number;
}

export class OutboundDrain {
  private draining = false;
  private tickStartedAt: number | null = null;
  /** Rows with a send promise that has not settled, in this process. */
  private readonly active = new Set<number>();
  private consecutiveFailures = 0;
  private firstFailureAt: number | null = null;
  /** The WhatsApp link is open. False until the first connection opens. */
  private up = false;
  /** Incremented on every link state change; a send that started in an
   *  earlier epoch lost its link. */
  private epoch = 0;

  constructor(private readonly d: DrainDeps) {}

  /** The connection opened: sends may start. Resets the rot count. */
  linkUp(): void {
    this.up = true;
    this.epoch += 1;
    this.resetRot();
  }

  /** The connection closed: no send starts until `linkUp`. Sends already
   *  pending that fail are not counted against their rows or the rot count. */
  linkDown(): void {
    this.up = false;
    this.epoch += 1;
    this.resetRot();
  }

  isLinkUp(): boolean {
    return this.up;
  }

  private resetRot(): void {
    this.consecutiveFailures = 0;
    this.firstFailureAt = null;
  }

  private countRot(): void {
    this.consecutiveFailures += 1;
    if (this.firstFailureAt === null) this.firstFailureAt = Date.now();
  }

  private rotReached(): boolean {
    return (
      this.consecutiveFailures >= CONNECTION_ROT_THRESHOLD &&
      this.firstFailureAt !== null &&
      Date.now() - this.firstFailureAt >= (this.d.rotMinSpanMs ?? ROT_MIN_SPAN_MS)
    );
  }

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
    if (this.draining || !this.up) return;
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
      if (!this.up) break;
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
      if (this.rotReached()) {
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

  /** Returns true when the tick should stop (a hung socket or a lost link). */
  private async sendRow(r: OutboundRow, jid: string, content: AnyMessageContent): Promise<boolean> {
    const msgId = this.d.store.markInFlight(r.id, r.msgId ?? this.d.newMessageId());
    const quoted = parseQuoted(r.quotedJson, jid);
    const epoch = this.epoch;
    const linkLost = () => !this.up || this.epoch !== epoch;
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
      // late result decide. A timeout during which the link closed is the
      // outage's, not the row's: no attempt, no rot count.
      const lost = linkLost();
      const { status } = this.d.store.markTimedOut(r.id, e.message, OUTBOUND_MAX_ATTEMPTS, !lost);
      if (!lost) this.countRot();
      void settled.then((late) => {
        if (late.ok) {
          this.d.store.markSent(r.id, late.sent?.key?.id ?? msgId);
          cleanupMedia(r);
          this.d.log.warn({ id: r.id }, "whatsapp: timed-out send completed late — marked sent");
        } else if (this.d.store.status(r.id) === "in_flight") {
          if (linkLost() || isLinkError(late.err)) {
            this.d.store.markLinkLost(r.id, late.err.message);
          } else {
            const { status: s } = this.d.store.markFailure(r.id, late.err.message, OUTBOUND_MAX_ATTEMPTS);
            if (s === "failed") cleanupMedia(r);
          }
        }
      });
      if (status === "failed") cleanupMedia(r);
      this.d.log.warn(
        { id: r.id, kind: r.kind, jid, consecutive: this.consecutiveFailures, linkLost: lost },
        "whatsapp: outbound send timed out — aborting tick (hung socket won't recover row-to-row)",
      );
      return true;
    }
    if (outcome.ok) {
      this.d.store.markSent(r.id, outcome.sent?.key?.id ?? msgId);
      cleanupMedia(r);
      this.resetRot();
      this.d.log.info({ id: r.id, kind: r.kind, target: r.target, jid }, "whatsapp: outbound sent");
      return false;
    }
    const err = outcome.err.message;
    if (linkLost() || isLinkError(outcome.err)) {
      // The link failed, not the message: back to pending with the same id,
      // no attempt used. Stop the tick; the next one runs only while the
      // link is up.
      this.d.store.markLinkLost(r.id, err);
      if (!linkLost()) {
        this.countRot();
        this.d.log.warn(
          { id: r.id, err, consecutive: this.consecutiveFailures, threshold: CONNECTION_ROT_THRESHOLD },
          "whatsapp: outbound send failed on the link while it reports open — connection-rot counter incremented",
        );
      } else {
        this.d.log.warn({ id: r.id, err }, "whatsapp: outbound send failed because the link closed — row kept pending, attempt not counted");
      }
      return true;
    }
    const { status } = this.d.store.markFailure(r.id, err, OUTBOUND_MAX_ATTEMPTS);
    if (status === "failed") cleanupMedia(r);
    this.d.log.warn({ id: r.id, err, attempts: r.attempts + 1 }, "whatsapp: outbound send failed");
    return false;
  }
}
