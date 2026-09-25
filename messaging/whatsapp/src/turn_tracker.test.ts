import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import { TurnTracker } from "./turn_tracker.js";

const vectors = JSON.parse(
  readFileSync(
    path.join(import.meta.dirname, "../../../core/testdata/turn_tracker_vectors.json"),
    "utf8",
  ),
) as { cases: Array<{ name: string; records: unknown[]; events: unknown[] }> };

test("TurnTracker matches the shared turn-tracker vectors", () => {
  assert.ok(vectors.cases.length > 0);
  for (const c of vectors.cases) {
    const t = new TurnTracker();
    const jsonl = c.records.map((r) => JSON.stringify(r) + "\n").join("");
    // Split mid-stream so partial-line buffering runs in every case.
    const cut = Math.floor(jsonl.length / 2);
    const got = [...t.feed(jsonl.slice(0, cut)), ...t.feed(jsonl.slice(cut))];
    assert.deepEqual(got, c.events, c.name);
  }
});

test("TurnTracker buffers a partial line", () => {
  const t = new TurnTracker();
  const rec = JSON.stringify({ type: "user", origin: { kind: "human" }, message: { content: "hi" } });
  assert.deepEqual(t.feed(rec.slice(0, 10)), []);
  assert.equal(t.feed(rec.slice(10) + "\n").length, 1);
  assert.equal(t.turnOpen(), true);
});
