// Outbound drain send protocol (ADR-033): in-flight idempotency (#3), quote
// validation (#19), the secret filter on every outbound text (#9).

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { BufferJSON, type AnyMessageContent, type WAMessage } from "@whiskeysockets/baileys";
import { ChatSessionStore, IN_FLIGHT_GRACE_MS, OutboundQueueStore } from "./db.js";
import { CONNECTION_ROT_THRESHOLD, OUTBOUND_MAX_ATTEMPTS, OutboundDrain, isLinkError, parseQuoted } from "./outbound_drain.js";
import { DatabaseSync } from "node:sqlite";
import { buildRules } from "./secret_filter.js";

const CHAT = "5511999999999@s.whatsapp.net";
// Synthetic ids built at runtime: the committed-secrets scanner reads a
// literal `<digits>@<domain>` or an address as real personal data.
const GROUP = ["120363000000000000", "g.us"].join("@");
const MAIL = ["someone", "corp.invalid"].join("@");
const HOME = ["", "Users", "testuser"].join("/");

interface Call {
  jid: string;
  content: AnyMessageContent;
  opts: { messageId: string; quoted?: WAMessage };
  resolve: (m: WAMessage | undefined) => void;
  reject: (e: Error) => void;
}

function setup(envText = "", opts: { rotMinSpanMs?: number; linkUp?: boolean } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-drain-"));
  const dbPath = path.join(dir, "whatsapp.db");
  new ChatSessionStore(dbPath);
  const store = new OutboundQueueStore(dbPath);
  const calls: Call[] = [];
  const warnings: string[] = [];
  const fatals: string[] = [];
  let ids = 0;
  const drain = new OutboundDrain({
    rotMinSpanMs: opts.rotMinSpanMs ?? 0,
    store,
    resolveTarget: (t) => (t === CHAT || t === GROUP ? t : null),
    send: (jid, content, opts) =>
      new Promise((resolve, reject) => {
        calls.push({ jid, content, opts, resolve, reject });
      }),
    newMessageId: () => `ID${++ids}`,
    rules: () => buildRules(envText, "SomeClientName\n", HOME),
    withheldNote: "({count} withheld)",
    mediaMaxBytes: 1024,
    log: { info() {}, warn: (_o, m) => warnings.push(m), error() {} },
    fatal: async (m) => {
      fatals.push(m);
    },
    timeoutFor: () => 50,
  });
  if (opts.linkUp ?? true) drain.linkUp();
  return { store, drain, calls, warnings, fatals, dbPath };
}

function attempts(dbPath: string, id: number): { attempts: number; status: string; last_error: string | null } {
  const db = new DatabaseSync(dbPath);
  try {
    return db.prepare("SELECT attempts, status, last_error FROM outbound_queue WHERE id = ?").get(id) as {
      attempts: number;
      status: string;
      last_error: string | null;
    };
  } finally {
    db.close();
  }
}

const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));

test("a timed-out send is not retried while it can still succeed; a late success marks it sent (#3)", async () => {
  const t = setup();
  const id = t.store.enqueue({ target: CHAT, body: "hello", source: "chat-reply" });
  await t.drain.tick(); // times out after 50ms
  assert.equal(t.calls.length, 1);
  assert.equal(t.store.status(id), "in_flight");
  await t.drain.tick();
  assert.equal(t.calls.length, 1, "no second send while the first can still succeed");
  t.calls[0].resolve({ key: { id: "ID1" } } as WAMessage);
  await wait(10);
  assert.equal(t.store.status(id), "sent");
  await t.drain.tick();
  assert.equal(t.calls.length, 1);
});

test("a retry after the grace period reuses the message id; a server ack marks the row sent (#3)", async () => {
  const t = setup();
  const id = t.store.enqueue({ target: CHAT, body: "hello", source: "chat-reply" });
  await t.drain.tick();
  const first = t.calls[0].opts.messageId;
  // The first send failed late (connection dropped).
  t.calls[0].reject(new Error("Connection Closed"));
  await wait(10);
  assert.equal(t.store.status(id), "pending");
  await t.drain.tick();
  assert.equal(t.calls.length, 2);
  assert.equal(t.calls[1].opts.messageId, first, "the same WhatsApp id on every attempt");
  // The process restarts with the row in flight: after the grace period it
  // is attempted again, still with the same id…
  const rows = t.store.pending(10, new Set(), Date.now() + IN_FLIGHT_GRACE_MS + 1_000);
  assert.equal(rows.length, 1);
  assert.equal(rows[0].msgId, first);
  // …unless the server acknowledged that id meanwhile.
  t.drain.onServerAck(first);
  assert.equal(t.store.status(id), "sent");
  assert.equal(t.store.pending(10, new Set(), Date.now() + IN_FLIGHT_GRACE_MS + 1_000).length, 0);
});

test("a quote is passed only when it is a complete message of the target chat (#19)", async () => {
  const good = JSON.stringify({ key: { id: "Q1", remoteJid: CHAT }, message: { conversation: "hi" } }, BufferJSON.replacer);
  const other = JSON.stringify({ key: { id: "Q2", remoteJid: GROUP }, message: { conversation: "x" } }, BufferJSON.replacer);
  const noMessage = JSON.stringify({ key: { id: "Q3", remoteJid: CHAT } });
  assert.equal(parseQuoted(good, CHAT)?.key.id, "Q1");
  assert.equal(parseQuoted(other, CHAT), undefined);
  assert.equal(parseQuoted(noMessage, CHAT), undefined);
  assert.equal(parseQuoted("not json", CHAT), undefined);
  const t = setup();
  t.store.enqueue({ target: CHAT, body: "a", source: "chat-reply", quotedJson: other });
  await t.drain.tick();
  assert.equal(t.calls[0].opts.quoted, undefined, "a foreign quote is dropped, the message is still sent");
});

test("the secret filter redacts credentials everywhere and withholds identifying progress (#9)", async () => {
  const t = setup("DISCORD_BOT_TOKEN=abcdefghijklmnop123\nWHATSAPP_ALLOWED_DM_JIDS=5511988887777\n");
  const reply = t.store.enqueue({ target: CHAT, body: "token abcdefghijklmnop123 and number 5511988887777", source: "chat-reply" });
  const progress = t.store.enqueue({ target: CHAT, body: "↻ Progress: calling SomeClientName", source: "chat-progress" });
  const group = t.store.enqueue({ target: GROUP, body: `mail me at ${MAIL}, SomeClientName`, source: "chat-reply" });
  for (let i = 0; i < 3; i++) {
    await t.drain.tick();
    for (const c of t.calls) c.resolve({ key: { id: c.opts.messageId } } as WAMessage);
    await wait(5);
  }
  const texts = t.calls.map((c) => (c.content as { text: string }).text);
  assert.equal(texts.length, 2, "the progress message was withheld");
  assert.equal(t.store.status(progress), "failed");
  assert.match(texts[0], /token \[redacted\] and number 5511988887777/, "a DM reply keeps the operator's own data");
  assert.match(texts[0], /\(1 withheld\)$/);
  assert.doesNotMatch(texts[1], /SomeClientName|someone@/, "a group message loses identifiers");
  assert.ok(t.store.status(reply) === "sent" && t.store.status(group) === "sent");
  assert.ok(t.warnings.some((w) => /withheld by the secret filter/.test(w)));
});

// ── Link gating (ADR-027 amendment, 2026-09) ─────────────────────────────

/** Run one tick; settle the send it starts with `settle` while it waits. */
async function tickSettling(t: ReturnType<typeof setup>, settle: (c: Call) => void): Promise<void> {
  const before = t.calls.length;
  const p = t.drain.tick();
  await wait(5);
  if (t.calls.length > before) settle(t.calls[t.calls.length - 1]);
  await p;
}

const closed = () => Object.assign(new Error("Connection Closed"), { output: { statusCode: 428 } });

test("isLinkError: closed/lost connections and link status codes, not message errors", () => {
  assert.equal(isLinkError(new Error("Connection Closed")), true);
  assert.equal(isLinkError(new Error("Connection Closed (no live socket)")), true);
  assert.equal(isLinkError(Object.assign(new Error("Timed Out"), { output: { statusCode: 408 } })), true);
  assert.equal(isLinkError(Object.assign(new Error("x"), { output: { statusCode: 503 } })), true);
  assert.equal(isLinkError(new Error("not-acceptable")), false);
  assert.equal(isLinkError(Object.assign(new Error("bad request"), { output: { statusCode: 400 } })), false);
});

test("the drain sends nothing until the link is up", async () => {
  const t = setup("", { linkUp: false });
  const id = t.store.enqueue({ target: CHAT, body: "hello", source: "reminder" });
  await t.drain.tick();
  assert.equal(t.calls.length, 0);
  assert.equal(t.store.status(id), "pending");
  t.drain.linkUp();
  await tickSettling(t, (c) => c.resolve({ key: { id: c.opts.messageId } } as WAMessage));
  assert.equal(t.calls.length, 1);
  assert.equal(t.store.status(id), "sent");
});

test("an outage does not use up a row's attempts; the row is sent with the same id after the link returns", async () => {
  const t = setup();
  const id = t.store.enqueue({ target: CHAT, body: "reminder", source: "reminder" });
  // The send fails because the socket closed; the close event follows.
  await tickSettling(t, (c) => c.reject(closed()));
  t.drain.linkDown();
  const firstId = t.calls[0].opts.messageId;
  // Before the fix the drain retried every second and failed the row after
  // five attempts. Now nothing is sent while the link is down.
  for (let i = 0; i < 10; i++) await t.drain.tick();
  assert.equal(t.calls.length, 1, "no send while the link is down");
  const row = attempts(t.dbPath, id);
  assert.equal(row.status, "pending");
  assert.equal(row.attempts, 0, "the link failure did not count");
  assert.match(row.last_error ?? "", /link down; attempt not counted/);
  t.drain.linkUp();
  await tickSettling(t, (c) => c.resolve({ key: { id: c.opts.messageId } } as WAMessage));
  assert.equal(t.calls.length, 2);
  assert.equal(t.calls[1].opts.messageId, firstId, "idempotent: the same WhatsApp id");
  assert.equal(t.store.status(id), "sent");
  assert.equal(t.fatals.length, 0);
});

test("many link outages in a row never fail the row or exit the process", async () => {
  const t = setup();
  const id = t.store.enqueue({ target: CHAT, body: "reminder", source: "reminder" });
  for (let outage = 0; outage < OUTBOUND_MAX_ATTEMPTS * 2; outage++) {
    // Up to threshold-1 failures on a link that still reports open, then the close.
    for (let i = 0; i < CONNECTION_ROT_THRESHOLD - 1; i++) await tickSettling(t, (c) => c.reject(closed()));
    t.drain.linkDown();
    t.drain.linkUp();
  }
  assert.equal(t.fatals.length, 0, "a close resets the rot count");
  const row = attempts(t.dbPath, id);
  assert.equal(row.status, "pending");
  assert.equal(row.attempts, 0);
});

test("a send pending when the link closes is not counted, whatever error it ends with", async () => {
  const t = setup();
  const id = t.store.enqueue({ target: CHAT, body: "hello", source: "chat-reply" });
  const p = t.drain.tick();
  await wait(5);
  t.drain.linkDown();
  // Not a link-shaped error, but the link closed while the send was pending.
  t.calls[0].reject(new Error("some socket error"));
  await p;
  const row = attempts(t.dbPath, id);
  assert.equal(row.status, "pending");
  assert.equal(row.attempts, 0);
});

test("a send that times out while the link closes is not counted; its late failure keeps the row pending", async () => {
  const t = setup();
  const id = t.store.enqueue({ target: CHAT, body: "hello", source: "chat-reply" });
  const p = t.drain.tick();
  await wait(5);
  t.drain.linkDown(); // the close event arrives while the send hangs
  await p; // times out after 50ms
  let row = attempts(t.dbPath, id);
  assert.equal(row.status, "in_flight", "the send may still succeed");
  assert.equal(row.attempts, 0);
  t.calls[0].reject(closed());
  await wait(10);
  row = attempts(t.dbPath, id);
  assert.equal(row.status, "pending");
  assert.equal(row.attempts, 0);
  assert.equal(t.fatals.length, 0);
});

test("link failures on a link that reports open are connection rot: exit after the threshold and the time span", async () => {
  const quick = setup();
  quick.store.enqueue({ target: CHAT, body: "hello", source: "chat-reply" });
  for (let i = 0; i < CONNECTION_ROT_THRESHOLD; i++) await tickSettling(quick, (c) => c.reject(closed()));
  assert.equal(quick.fatals.length, 1, "a zombie socket still exits for a launchd respawn");

  // With a span, the same failures within a few ms do not exit: the close
  // event may not have arrived yet.
  const spanned = setup("", { rotMinSpanMs: 60_000 });
  spanned.store.enqueue({ target: CHAT, body: "hello", source: "chat-reply" });
  for (let i = 0; i < CONNECTION_ROT_THRESHOLD + 2; i++) await tickSettling(spanned, (c) => c.reject(closed()));
  assert.equal(spanned.fatals.length, 0);
});

test("a message error (not the link) still counts attempts and fails the row at the limit", async () => {
  const t = setup();
  const id = t.store.enqueue({ target: CHAT, body: "hello", source: "chat-reply" });
  for (let i = 0; i < OUTBOUND_MAX_ATTEMPTS; i++) await tickSettling(t, (c) => c.reject(new Error("not-acceptable")));
  const row = attempts(t.dbPath, id);
  assert.equal(row.status, "failed");
  assert.equal(row.attempts, OUTBOUND_MAX_ATTEMPTS);
  assert.equal(t.fatals.length, 0);
});
