import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { JobStore, withQuickWindow } from "./jobs.js";

function freshStore(): JobStore {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-jobs-"));
  return new JobStore({ dbPath: path.join(dir, "jobs.db") });
}

test("job lifecycle: insert → session → promoted → done", () => {
  const store = freshStore();
  const id = store.insert({ kind: "act", chatId: "123@lid", instruction: "summarize the doc" });
  store.setSession(id, "sess-uuid", "@7");
  store.markPromoted(id);
  store.markDone(id, "the summary");
  const row = store.recent().find((r) => r.id === id)!;
  assert.equal(row.status, "done");
  assert.equal(row.sessionId, "sess-uuid");
  assert.ok(row.promotedAt);
  assert.ok(row.finishedAt);
  assert.equal(row.resultSummary, "the summary");
});

test("markPromoted is idempotent (keeps the first timestamp)", async () => {
  const store = freshStore();
  const id = store.insert({ kind: "act", chatId: "1", instruction: "x" });
  store.markPromoted(id);
  const first = store.recent()[0].promotedAt;
  await new Promise((r) => setTimeout(r, 5));
  store.markPromoted(id);
  assert.equal(store.recent()[0].promotedAt, first);
});

test("markFailed records error and result_summary caps at 4000", () => {
  const store = freshStore();
  const a = store.insert({ kind: "enrich", chatId: "1", instruction: "x" });
  store.markFailed(a, "boom");
  assert.equal(store.recent().find((r) => r.id === a)!.error, "boom");
  const b = store.insert({ kind: "act", chatId: "1", instruction: "x" });
  store.markDone(b, "y".repeat(9000));
  assert.equal(store.recent().find((r) => r.id === b)!.resultSummary!.length, 4000);
});

test("sweepOrphans flips only running rows and returns them", () => {
  const store = freshStore();
  const running = store.insert({ kind: "act", chatId: "1", instruction: "in flight" });
  const done = store.insert({ kind: "act", chatId: "1", instruction: "finished" });
  store.markDone(done, "ok");
  const swept = store.sweepOrphans();
  assert.deepEqual(swept.map((r) => r.id), [running]);
  const after = store.recent();
  assert.equal(after.find((r) => r.id === running)!.status, "orphaned");
  assert.equal(after.find((r) => r.id === done)!.status, "done");
  // Second sweep finds nothing.
  assert.equal(store.sweepOrphans().length, 0);
});

test("withQuickWindow: in-window settle returns the value", async () => {
  const result = await withQuickWindow(Promise.resolve(42), 1000);
  assert.deepEqual(result, { settled: true, value: 42 });
});

test("withQuickWindow: timeout returns settled:false without consuming p", async () => {
  let resolveLate!: (v: string) => void;
  const late = new Promise<string>((r) => (resolveLate = r));
  const result = await withQuickWindow(late, 20);
  assert.deepEqual(result, { settled: false });
  // The original promise is still usable by the deferred path.
  resolveLate("late value");
  assert.equal(await late, "late value");
});

test("withQuickWindow: in-window rejection propagates to the caller", async () => {
  await assert.rejects(
    withQuickWindow(Promise.reject(new Error("sync fail")), 1000),
    /sync fail/,
  );
});

test("withQuickWindow: post-window rejection does not crash unhandled", async () => {
  let rejectLate!: (e: Error) => void;
  const late = new Promise<string>((_, rj) => (rejectLate = rj));
  const result = await withQuickWindow(late, 20);
  assert.deepEqual(result, { settled: false });
  // Caller attaches its own handler (the deferred path) — and the helper's
  // internal derived branch must not blow up the process.
  const handled = late.catch((e) => `handled: ${e.message}`);
  rejectLate(new Error("late fail"));
  assert.equal(await handled, "handled: late fail");
  // Give the loop a tick so an unhandled rejection (if any) would fire.
  await new Promise((r) => setTimeout(r, 10));
});
