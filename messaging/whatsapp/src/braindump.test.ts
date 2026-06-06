import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { localToday, applyPlan, type CaptureOp } from "./braindump.js";
import { ChatSessionStore, PendingPlansStore } from "./db.js";
import type { Config } from "./config.js";

function tmpVault(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "braindump-test-"));
  for (const b of ["0-Inbox", "2-Daily-Notes", "5-Resources"]) {
    fs.mkdirSync(path.join(dir, b), { recursive: true });
  }
  return dir;
}

function seedPlan(dbPath: string, ops: CaptureOp[]): { store: PendingPlansStore; planId: string } {
  // ChatSessionStore's constructor creates the pending_plans table.
  new ChatSessionStore(dbPath);
  const store = new PendingPlansStore(dbPath);
  const planId = store.insert({
    chatId: "test@chat",
    captureText: "irrelevant",
    inputKind: "voice",
    opsJson: JSON.stringify(ops),
    summary: "test plan",
    confidence: 0.9,
  });
  return { store, planId };
}

test("localToday returns an ISO YYYY-MM-DD date", () => {
  assert.match(localToday(), /^\d{4}-\d{2}-\d{2}$/);
});

test("modify patch re-dates a daily note: filename + synced frontmatter, old file absent", () => {
  const vault = tmpVault();
  const dbPath = path.join(vault, ".test.db");
  const op: CaptureOp = {
    op: "create",
    bucket: "2-Daily-Notes",
    filename: "2026-06-05.md",
    body: "---\ncreated: 2026-06-05\nsource: whatsapp-braindump\ntags: [daily]\n---\n\n# 2026-06-05 — log\n\nfoo\n",
    createsSubfolder: false,
    reason: "daily note",
  };
  const { store, planId } = seedPlan(dbPath, [op]);
  const config = { vaultPath: vault } as unknown as Config;

  const outcome = applyPlan(planId, [1], store, config, [{ id: 1, filename: "2026-06-04.md" }]);

  assert.equal(outcome.ops[0].status, "ok");
  assert.equal(outcome.ops[0].resultPath, "2-Daily-Notes/2026-06-04.md");
  assert.ok(!fs.existsSync(path.join(vault, "2-Daily-Notes/2026-06-05.md")), "stale-dated file must not exist");
  const written = fs.readFileSync(path.join(vault, "2-Daily-Notes/2026-06-04.md"), "utf-8");
  assert.match(written, /created: 2026-06-04/, "frontmatter created must be re-synced");
  assert.ok(!written.includes("created: 2026-06-05"), "old created date must be gone");

  // Patched ops are persisted back for audit.
  const persisted = JSON.parse(store.get(planId)!.opsJson) as CaptureOp[];
  assert.equal((persisted[0] as Extract<CaptureOp, { op: "create" }>).filename, "2026-06-04.md");

  fs.rmSync(vault, { recursive: true, force: true });
});

test("modify patch retargets a create op's bucket", () => {
  const vault = tmpVault();
  const dbPath = path.join(vault, ".test.db");
  const op: CaptureOp = {
    op: "create",
    bucket: "0-Inbox",
    filename: "note.md",
    body: "---\ncreated: 2026-06-04\n---\n\nbody\n",
    createsSubfolder: false,
    reason: "misc",
  };
  const { store, planId } = seedPlan(dbPath, [op]);
  const config = { vaultPath: vault } as unknown as Config;

  const outcome = applyPlan(planId, [1], store, config, [{ id: 1, bucket: "5-Resources" }]);

  assert.equal(outcome.ops[0].status, "ok");
  assert.equal(outcome.ops[0].resultPath, "5-Resources/note.md");
  assert.ok(fs.existsSync(path.join(vault, "5-Resources/note.md")));
  assert.ok(!fs.existsSync(path.join(vault, "0-Inbox/note.md")));

  fs.rmSync(vault, { recursive: true, force: true });
});

test("empty patches behaves exactly like plain apply", () => {
  const vault = tmpVault();
  const dbPath = path.join(vault, ".test.db");
  const op: CaptureOp = {
    op: "create",
    bucket: "0-Inbox",
    filename: "plain.md",
    body: "---\ncreated: 2026-06-04\n---\n\nbody\n",
    createsSubfolder: false,
    reason: "misc",
  };
  const { store, planId } = seedPlan(dbPath, [op]);
  const config = { vaultPath: vault } as unknown as Config;

  const outcome = applyPlan(planId, [1], store, config);

  assert.equal(outcome.ops[0].status, "ok");
  assert.equal(outcome.ops[0].resultPath, "0-Inbox/plain.md");

  fs.rmSync(vault, { recursive: true, force: true });
});
