// Sent-message store for Baileys' getMessage (ADR-027 amendment, 2026-09).

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import type { WAMessage, WAMessageKey } from "@whiskeysockets/baileys";
import { ChatSessionStore } from "./db.js";
import { SENT_RETENTION_MS, SentMessageStore } from "./sent_store.js";

const CHAT = "5511999999999@s.whatsapp.net";

function setup() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-sent-"));
  const dbPath = path.join(dir, "whatsapp.db");
  new ChatSessionStore(dbPath);
  return { dbPath, sent: new SentMessageStore(dbPath) };
}

/** The getMessage callback index.ts passes to makeWASocket. */
const getMessageFor = (sent: SentMessageStore) => async (key: WAMessageKey) => sent.get(key.id);

test("a sent message is returned by getMessage with the same content; a miss is undefined", async () => {
  const { sent } = setup();
  const msg = {
    key: { id: "3EB0SYNTHETIC0001", remoteJid: CHAT, fromMe: true },
    message: { extendedTextMessage: { text: "hello from the bot" } },
  } as WAMessage;
  assert.equal(sent.record(msg), true);
  const getMessage = getMessageFor(sent);
  const got = await getMessage({ id: "3EB0SYNTHETIC0001", remoteJid: CHAT, fromMe: true });
  assert.equal(got?.extendedTextMessage?.text, "hello from the bot");
  assert.equal(await getMessage({ id: "UNKNOWN", remoteJid: CHAT, fromMe: true }), undefined);
  assert.equal(await getMessage({ remoteJid: CHAT }), undefined);
});

test("messages without id, chat or content are not stored; a second record keeps the first", () => {
  const { sent } = setup();
  assert.equal(sent.record(undefined), false);
  assert.equal(sent.record({ key: { id: "A", remoteJid: CHAT } } as WAMessage), false);
  assert.equal(sent.record({ key: { remoteJid: CHAT }, message: { conversation: "x" } } as WAMessage), false);
  assert.equal(sent.record({ key: { id: "B", remoteJid: CHAT }, message: { conversation: "first" } } as WAMessage), true);
  assert.equal(sent.record({ key: { id: "B", remoteJid: CHAT }, message: { conversation: "second" } } as WAMessage), false);
  assert.equal(sent.get("B")?.conversation, "first");
});

test("the store is shared across store instances (a reconnect or send.ts)", () => {
  const { dbPath, sent } = setup();
  sent.record({ key: { id: "C", remoteJid: CHAT }, message: { conversation: "kept" } } as WAMessage);
  assert.equal(new SentMessageStore(dbPath).get("C")?.conversation, "kept");
});

test("prune removes messages older than the retention", () => {
  const { dbPath, sent } = setup();
  const now = Date.now();
  sent.record({ key: { id: "OLD", remoteJid: CHAT }, message: { conversation: "old" } } as WAMessage, now - SENT_RETENTION_MS - 60_000);
  sent.record({ key: { id: "NEW", remoteJid: CHAT }, message: { conversation: "new" } } as WAMessage, now - 60_000);
  assert.equal(sent.prune(SENT_RETENTION_MS, now), 1);
  assert.equal(sent.get("OLD"), undefined);
  assert.equal(sent.get("NEW")?.conversation, "new");
  const db = new DatabaseSync(dbPath);
  assert.equal((db.prepare("SELECT count(*) AS n FROM sent_messages").get() as { n: number }).n, 1);
  db.close();
});
