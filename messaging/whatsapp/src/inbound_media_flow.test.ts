// Inbound media (ADR-018, ADR-013) handled again after a crash AFTER its
// hand-off (ADR-033): the archive, its replies and its jobs happened, the
// bot stopped before recording the message as handled, and WhatsApp
// delivers it again. Nothing new is queued, archived or started.

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import type { WAMessage } from "@whiskeysockets/baileys";
import { ChatSessionStore, OutboundQueueStore } from "./db.js";
import { DocStore } from "./docstore.js";
import { handleInboundMedia, type MediaDeps } from "./inbound_media_flow.js";
import { JobStore } from "./jobs.js";
import { DEFAULT_TEXTS } from "./texts.js";

// A synthetic DM chat id built at runtime (the secrets scanner reads a
// literal JID as a real identifier).
const DM = ["5500000000000", "s.whatsapp.net"].join("@");

function setup() {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-media-"));
  const dbPath = path.join(dir, "whatsapp.db");
  new ChatSessionStore(dbPath);
  const outbound = new OutboundQueueStore(dbPath);
  const jobsDb = path.join(dir, "jobs.db");
  const jobStore = new JobStore({ dbPath: jobsDb });
  const docStore = new DocStore({ dbPath: path.join(dir, "documents.db"), documentsDir: path.join(dir, "docs") });
  const calls = { act: 0, import: 0, enrich: 0 };
  const d: MediaDeps = {
    mediaMaxBytes: 1024 * 1024,
    docStore,
    jobStore,
    outbound,
    texts: DEFAULT_TEXTS,
    formatReply: (b) => b,
    download: async () => Buffer.from("file bytes"),
    presence: async () => {},
    fireEnrich: (record, chatId, sourceKey) => {
      calls.enrich += 1;
      if (sourceKey && !jobStore.bySourceKey(sourceKey)) {
        jobStore.insert({ kind: "enrich", chatId, docId: record.id, instruction: "enrich", sourceKey });
      }
    },
    startAct: (o) => {
      calls.act += 1;
      const jobId = jobStore.insert({ kind: "act", chatId: o.chatId, docId: o.record.id, instruction: o.instruction, sourceKey: o.sourceKey });
      jobStore.markDone(jobId, "the total is 42");
      return { jobId, promise: Promise.resolve({ jobId, reply: "the total is 42", elapsedMs: 1 }) };
    },
    runImport: async (record, chatId, sourceKey) => {
      calls.import += 1;
      const jobId = jobStore.insert({ kind: "vault-import", chatId, docId: record.id, instruction: "import", sourceKey });
      jobStore.markDone(jobId, "{}");
      docStore.recordImport(record.id, "5-Resources/Imported/x.md", chatId);
      return "imported";
    },
    quickWindow: async (p) => ({ settled: true, value: await p }),
    log: { info() {}, warn() {}, error() {} },
  };
  const rows = () => outbound.pending(100).length;
  const jobs = () => (new DatabaseSync(jobsDb).prepare(`SELECT COUNT(*) AS n FROM jobs`).get() as { n: number }).n;
  return { d, rows, jobs, calls };
}

const doc = (id: string, caption: string) =>
  ({
    key: { id, remoteJid: DM },
    message: { documentMessage: { fileName: "receipt.pdf", mimetype: "application/pdf", fileLength: 10, caption } },
  }) as unknown as WAMessage;

test("an archived document handled again queues no second reply and starts no second enrichment", async () => {
  const t = setup();
  await handleInboundMedia(t.d, doc("M1", ""), DM, "dm");
  const rows = t.rows();
  await handleInboundMedia(t.d, doc("M1", ""), DM, "dm");
  assert.equal(t.rows(), rows);
  assert.equal(t.jobs(), 1, "one enrichment job");
});

test("an act-on-media message handled again starts no second job and queues no second result", async () => {
  const t = setup();
  await handleInboundMedia(t.d, doc("M2", "act: what is the total?"), DM, "dm");
  const rows = t.rows();
  const jobs = t.jobs();
  await handleInboundMedia(t.d, doc("M2", "act: what is the total?"), DM, "dm");
  assert.equal(t.calls.act, 1, "the act job is not started again");
  assert.equal(t.jobs(), jobs);
  assert.equal(t.rows(), rows);
});

test("a vault-import message handled again imports nothing again", async () => {
  const t = setup();
  await handleInboundMedia(t.d, doc("M3", "vault: receipt"), DM, "dm");
  const rows = t.rows();
  await handleInboundMedia(t.d, doc("M3", "vault: receipt"), DM, "dm");
  assert.equal(t.calls.import, 1, "the import job is not started again");
  assert.equal(t.rows(), rows);
});

test("a download failure handled again queues one note", async () => {
  const t = setup();
  t.d.download = async () => {
    throw new Error("media expired");
  };
  await handleInboundMedia(t.d, doc("M4", ""), DM, "dm");
  await handleInboundMedia(t.d, doc("M4", ""), DM, "dm");
  assert.equal(t.rows(), 1);
});

test("a document archived before a crash that skipped its enrichment is enriched on replay", async () => {
  const t = setup();
  const realEnrich = t.d.fireEnrich;
  t.d.fireEnrich = () => {}; // the process stopped after the archive commit
  await handleInboundMedia(t.d, doc("M9", ""), DM, "dm");
  assert.equal(t.jobs(), 0, "no enrichment before the crash");
  t.d.fireEnrich = realEnrich;
  await handleInboundMedia(t.d, doc("M9", ""), DM, "dm");
  assert.equal(t.jobs(), 1, "the replay starts the missing enrichment");
  await handleInboundMedia(t.d, doc("M9", ""), DM, "dm");
  assert.equal(t.jobs(), 1, "a further replay adds nothing");
});
