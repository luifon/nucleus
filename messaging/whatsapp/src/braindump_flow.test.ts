// Brain-dump flow (ADR-005a, ADR-033): every reply goes through the outbound
// queue callback, never the socket.

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import { DatabaseSync } from "node:sqlite";
import os from "node:os";
import path from "node:path";
import type { WAMessage } from "@whiskeysockets/baileys";
import { applyPlan, type CaptureOp } from "./braindump.js";
import { handleBrainDump, sweepExpiredPlans, type BraindumpDeps } from "./braindump_flow.js";
import type { Config } from "./config.js";
import { ChatSessionStore, OutboundQueueStore, PendingPlansStore } from "./db.js";
import { DEFAULT_TEXTS } from "./texts.js";

// A synthetic group id built at runtime: the committed-secrets scanner reads
// a literal group JID as a real identifier.
const GROUP = ["120363000000000000", "g.us"].join("@");

function setup(over: Partial<BraindumpDeps> = {}, ops: CaptureOp[] = [{ op: "create", bucket: "0-Inbox", filename: "n.md", body: "b", reason: "r" } as any]) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-braindump-"));
  const dbPath = path.join(dir, "whatsapp.db");
  const vault = path.join(dir, "vault");
  for (const b of ["0-Inbox", "4-Areas"]) fs.mkdirSync(path.join(vault, b), { recursive: true });
  new ChatSessionStore(dbPath);
  const plansStore = new PendingPlansStore(dbPath);
  const outbound = new OutboundQueueStore(dbPath);
  const calls = { plan: 0, interpret: 0 };
  const d: BraindumpDeps = {
    plansStore,
    texts: DEFAULT_TEXTS,
    // The bot binds this to the outbound queue; the test does the same.
    send: (chatId, body, dedupKey) => void outbound.enqueue({ target: chatId, body, source: "braindump", dedupKey }),
    presence: async () => {},
    extractText: (m) => m.message?.conversation ?? "",
    transcribeVoice: async () => {
      throw new Error("whisper missing");
    },
    planCapture: async (text, inputKind, chatId, sourceMsgId) => {
      calls.plan += 1;
      const planId = plansStore.insert({ chatId, captureText: text, inputKind, opsJson: JSON.stringify(ops), summary: "one note", confidence: 0.9, sourceMsgId });
      return {
        planId,
        shortId: planId.slice(0, 4),
        summary: "one note",
        confidence: 0.9,
        ops: ops.map((op, i) => ({ id: i + 1, op })),
        elapsedMs: 5,
      };
    },
    interpretResponse: async () => {
      calls.interpret += 1;
      return { action: "reject" };
    },
    applyPlan: (planId, ids, patches, byMsgId) =>
      applyPlan(planId, ids, plansStore, { vaultPath: vault } as unknown as Config, patches, byMsgId),
    diary: () => {},
    log: { info() {}, warn() {}, error() {} },
    ...over,
  };
  const rows = () => outbound.pending(100).map((r) => ({ target: r.target, body: r.body, source: r.source }));
  const plans = () => (new DatabaseSync(dbPath).prepare(`SELECT COUNT(*) AS n FROM pending_plans`).get() as { n: number }).n;
  return { d, rows, plansStore, calls, vault, plans };
}

const text = (t: string, id = "B1") => ({ key: { id, remoteJid: GROUP }, message: { conversation: t } }) as unknown as WAMessage;
const voice = { key: { id: "V1", remoteJid: GROUP }, message: { audioMessage: { seconds: 3 } } } as unknown as WAMessage;

test("a new capture's acknowledgement and rundown are queued, not sent on the socket", async () => {
  const t = setup();
  await handleBrainDump(t.d, text("remember the dentist"), GROUP);
  const rows = t.rows();
  assert.equal(rows[0].body, DEFAULT_TEXTS.received);
  assert.ok(rows.some((r) => r.body.includes("one note")), JSON.stringify(rows));
  assert.ok(rows.every((r) => r.target === GROUP && r.source === "braindump"));
});

test("a reply to a pending plan and a failed transcription are queued too", async () => {
  const t = setup();
  await handleBrainDump(t.d, text("capture"), GROUP);
  const before = t.rows().length;
  await handleBrainDump(t.d, text("no, drop it", "B2"), GROUP);
  assert.ok(t.rows().length > before, "the interpret acknowledgement and the cancel notice");
  await handleBrainDump(t.d, voice, GROUP);
  assert.ok(t.rows().some((r) => r.body.includes("whisper missing")));
});

test("expired plans are announced through the queue", async () => {
  const t = setup();
  await handleBrainDump(t.d, text("capture"), GROUP);
  const n = t.rows().length;
  sweepExpiredPlans(t.d, -1);
  assert.equal(t.rows().length, n + 1);
});

// ── ADR-033: a message handled again after a crash AFTER its hand-off ──
// (the flow ran, the bot stopped before recording the message as handled,
// WhatsApp delivered it again) creates nothing new.

test("a capture handled again after its plan and rundown were queued plans nothing and queues nothing", async () => {
  const t = setup();
  await handleBrainDump(t.d, text("remember the dentist", "C1"), GROUP);
  const rows = t.rows().length;
  await handleBrainDump(t.d, text("remember the dentist", "C1"), GROUP);
  assert.equal(t.calls.plan, 1, "planning is not run again");
  assert.equal(t.plans(), 1, "no second plan");
  assert.equal(t.rows().length, rows, "no second acknowledgement or rundown");
});

test("an accepted plan handled again files nothing again and queues nothing", async () => {
  const t = setup({ interpretResponse: async () => ({ action: "apply" }) });
  await handleBrainDump(t.d, text("remember the dentist", "C1"), GROUP);
  await handleBrainDump(t.d, text("ok", "C2"), GROUP);
  const note = path.join(t.vault, "0-Inbox", "n.md");
  const filed = fs.readFileSync(note, "utf8");
  const rows = t.rows().length;
  await handleBrainDump(t.d, text("ok", "C2"), GROUP);
  assert.equal(fs.readFileSync(note, "utf8"), filed, "the note is not written a second time");
  assert.equal(t.rows().length, rows, "no second outcome");
  assert.equal(t.plans(), 1, "the reply is not taken for a new capture");
});

test("a rejection handled again is not taken for a new capture", async () => {
  const t = setup();
  await handleBrainDump(t.d, text("remember the dentist", "C1"), GROUP);
  await handleBrainDump(t.d, text("no, drop it", "C2"), GROUP);
  const rows = t.rows().length;
  await handleBrainDump(t.d, text("no, drop it", "C2"), GROUP);
  assert.equal(t.calls.interpret, 1);
  assert.equal(t.calls.plan, 1);
  assert.equal(t.rows().length, rows);
});

test("an apply cut short resumes: finished ops are not filed again", async () => {
  const locked = { op: "append", targetPath: "4-Areas/log.md", body: "second", reason: "r" } as any;
  let interpreted = 0;
  const t = setup({ interpretResponse: async () => (interpreted++, { action: "apply" }) }, [
    { op: "create", bucket: "0-Inbox", filename: "first.md", body: "first", reason: "r" } as any,
    locked,
  ]);
  const log = path.join(t.vault, "4-Areas", "log.md");
  fs.writeFileSync(log, "existing\n");
  fs.chmodSync(log, 0o444); // the second op fails: the bot "stops" part-way
  await handleBrainDump(t.d, text("remember two things", "C1"), GROUP);
  await handleBrainDump(t.d, text("ok", "C2"), GROUP);
  const first = path.join(t.vault, "0-Inbox", "first.md");
  assert.equal(fs.readFileSync(first, "utf8"), "first\n");
  fs.chmodSync(log, 0o644);
  await handleBrainDump(t.d, text("ok", "C2"), GROUP);
  assert.equal(fs.readFileSync(first, "utf8"), "first\n", "the finished op is not filed again");
  assert.match(fs.readFileSync(log, "utf8"), /second/);
  assert.equal(interpreted, 1, "the reply is not interpreted again");
  assert.ok(t.rows().some((r) => r.body.includes("4-Areas/log.md")), "the outcome is queued");
});
