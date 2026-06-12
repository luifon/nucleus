import { test } from "node:test";
import assert from "node:assert/strict";
import { DatabaseSync } from "node:sqlite";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { ChatSessionStore, OutboundQueueStore } from "./db.js";

function tmpDb(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-dbtest-"));
  return path.join(dir, "test.db");
}

/** Hand-build a pre-ADR-018 (10-column) outbound_queue. */
function buildLegacyDb(dbPath: string): void {
  const db = new DatabaseSync(dbPath);
  db.exec(`
    CREATE TABLE outbound_queue (
      id INTEGER PRIMARY KEY AUTOINCREMENT,
      target TEXT NOT NULL,
      body TEXT NOT NULL,
      source TEXT NOT NULL,
      enqueued_at TEXT NOT NULL,
      status TEXT NOT NULL DEFAULT 'pending',
      attempts INTEGER NOT NULL DEFAULT 0,
      last_error TEXT,
      sent_at TEXT,
      msg_id TEXT
    );
  `);
  db.prepare(
    `INSERT INTO outbound_queue (target, body, source, enqueued_at)
     VALUES ('123', 'legacy row', 'test', '2026-01-01T00:00:00Z')`,
  ).run();
  db.close();
}

test("ChatSessionStore heals a pre-media outbound_queue via PRAGMA detection", () => {
  const dbPath = tmpDb();
  buildLegacyDb(dbPath);
  new ChatSessionStore(dbPath); // runs addColumnsIfMissing
  const db = new DatabaseSync(dbPath);
  const cols = (db.prepare("PRAGMA table_info(outbound_queue)").all() as Array<{ name: string }>)
    .map((r) => r.name);
  for (const c of ["kind", "media_path", "mimetype", "filename"]) {
    assert.ok(cols.includes(c), `missing column ${c}`);
  }
  // Legacy row defaulted to kind='text'.
  const legacy = db.prepare("SELECT kind FROM outbound_queue WHERE body = 'legacy row'").get() as {
    kind: string;
  };
  assert.equal(legacy.kind, "text");
  db.close();
});

test("media enqueue round-trips through pending()", () => {
  const dbPath = tmpDb();
  new ChatSessionStore(dbPath);
  const q = new OutboundQueueStore(dbPath);
  const id = q.enqueue({
    target: "123",
    body: "a caption",
    source: "test",
    kind: "document",
    mediaPath: "/tmp/staged/x.pdf",
    mimetype: "application/pdf",
    filename: "x.pdf",
  });
  const rows = q.pending();
  const row = rows.find((r) => r.id === id)!;
  assert.equal(row.kind, "document");
  assert.equal(row.mediaPath, "/tmp/staged/x.pdf");
  assert.equal(row.mimetype, "application/pdf");
  assert.equal(row.filename, "x.pdf");
  assert.equal(row.body, "a caption");
});

test("legacy text enqueue still works and reads back kind='text'", () => {
  const dbPath = tmpDb();
  new ChatSessionStore(dbPath);
  const q = new OutboundQueueStore(dbPath);
  const id = q.enqueue({ target: "123", body: "hi", source: "test" });
  const row = q.pending().find((r) => r.id === id)!;
  assert.equal(row.kind, "text");
  assert.equal(row.mediaPath, null);
});

test("markFailure reports terminality; markFailedTerminal is immediate", () => {
  const dbPath = tmpDb();
  new ChatSessionStore(dbPath);
  const q = new OutboundQueueStore(dbPath);
  const id = q.enqueue({ target: "123", body: "hi", source: "test" });
  assert.equal(q.markFailure(id, "boom", 3).status, "pending"); // attempt 1
  assert.equal(q.markFailure(id, "boom", 3).status, "pending"); // attempt 2
  assert.equal(q.markFailure(id, "boom", 3).status, "failed"); // attempt 3 = terminal

  const id2 = q.enqueue({ target: "123", body: "hi2", source: "test" });
  q.markFailedTerminal(id2, "file gone");
  assert.ok(!q.pending().some((r) => r.id === id2), "terminal row must leave pending");
});

test("pendingMediaPaths returns only pending media rows", () => {
  const dbPath = tmpDb();
  new ChatSessionStore(dbPath);
  const q = new OutboundQueueStore(dbPath);
  q.enqueue({ target: "1", body: "text row", source: "t" });
  const keep = q.enqueue({
    target: "1", body: "", source: "t",
    kind: "image", mediaPath: "/tmp/keep.png",
  });
  const gone = q.enqueue({
    target: "1", body: "", source: "t",
    kind: "image", mediaPath: "/tmp/gone.png",
  });
  q.markSent(gone, "m1");
  const paths = q.pendingMediaPaths();
  assert.deepEqual(paths, ["/tmp/keep.png"]);
  assert.ok(keep > 0);
});
