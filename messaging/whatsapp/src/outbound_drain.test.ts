// Outbound drain send protocol (ADR-033): in-flight idempotency (#3), quote
// validation (#19), the secret filter on every outbound text (#9).

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { BufferJSON, type AnyMessageContent, type WAMessage } from "@whiskeysockets/baileys";
import { ChatSessionStore, IN_FLIGHT_GRACE_MS, OutboundQueueStore } from "./db.js";
import { OutboundDrain, parseQuoted } from "./outbound_drain.js";
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

function setup(envText = "") {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-drain-"));
  const dbPath = path.join(dir, "whatsapp.db");
  new ChatSessionStore(dbPath);
  const store = new OutboundQueueStore(dbPath);
  const calls: Call[] = [];
  const warnings: string[] = [];
  let ids = 0;
  const drain = new OutboundDrain({
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
    fatal: async () => {},
    timeoutFor: () => 50,
  });
  return { store, drain, calls, warnings };
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
