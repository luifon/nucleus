import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import {
  buildOutboundContent,
  DRAIN_WATCHDOG_MS,
  MAX_MEDIA_SENDS_PER_TICK,
  SEND_TIMEOUT_MEDIA_MS,
  SEND_TIMEOUT_TEXT_MS,
  sendTimeoutFor,
  sweepOutboundStaging,
} from "./outbound.js";
import type { OutboundRow } from "./db.js";

function row(over: Partial<OutboundRow>): OutboundRow {
  return {
    id: 1,
    target: "123",
    body: "",
    source: "test",
    enqueuedAt: "2026-01-01T00:00:00Z",
    attempts: 0,
    kind: "text",
    mediaPath: null,
    mimetype: null,
    filename: null,
    ...over,
  };
}

function tmpFile(contents = "x"): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-outtest-"));
  const p = path.join(dir, "f.bin");
  fs.writeFileSync(p, contents);
  return p;
}

test("text rows build a text payload", () => {
  const c = buildOutboundContent(row({ body: "hello" }));
  assert.deepEqual(c, { text: "hello" });
});

test("image rows build the {url} stream form with optional caption", () => {
  const p = tmpFile();
  const c = buildOutboundContent(
    row({ kind: "image", mediaPath: p, body: "cap", mimetype: "image/png" }),
  ) as any;
  assert.deepEqual(c.image, { url: p });
  assert.equal(c.caption, "cap");
  assert.equal(c.mimetype, "image/png");
  const noCap = buildOutboundContent(row({ kind: "image", mediaPath: p })) as any;
  assert.equal(noCap.caption, undefined);
});

test("document rows carry mimetype + fileName with fallbacks", () => {
  const p = tmpFile();
  const c = buildOutboundContent(row({ kind: "document", mediaPath: p })) as any;
  assert.equal(c.mimetype, "application/octet-stream");
  assert.equal(c.fileName, path.basename(p));
  const named = buildOutboundContent(
    row({ kind: "document", mediaPath: p, mimetype: "application/pdf", filename: "rg.pdf" }),
  ) as any;
  assert.equal(named.mimetype, "application/pdf");
  assert.equal(named.fileName, "rg.pdf");
});

test("missing or oversized media files are non-retryable errors", () => {
  const missing = buildOutboundContent(row({ kind: "image", mediaPath: "/nope/x.png" }));
  assert.ok("error" in missing && missing.error.includes("missing"));
  const p = tmpFile("0123456789");
  const oversized = buildOutboundContent(row({ kind: "document", mediaPath: p }), 5);
  assert.ok("error" in oversized && oversized.error.includes("too large"));
  const noPath = buildOutboundContent(row({ kind: "document" }));
  assert.ok("error" in noPath && noPath.error.includes("no media_path"));
});

test("watchdog is derived from the per-kind timeouts", () => {
  assert.equal(sendTimeoutFor("text"), SEND_TIMEOUT_TEXT_MS);
  assert.equal(sendTimeoutFor("image"), SEND_TIMEOUT_MEDIA_MS);
  assert.equal(sendTimeoutFor("document"), SEND_TIMEOUT_MEDIA_MS);
  assert.equal(
    DRAIN_WATCHDOG_MS,
    (20 - MAX_MEDIA_SENDS_PER_TICK) * SEND_TIMEOUT_TEXT_MS +
      MAX_MEDIA_SENDS_PER_TICK * SEND_TIMEOUT_MEDIA_MS +
      60_000,
  );
});

test("staging sweep keeps pending paths, removes orphans, never escapes the dir", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-sweep-"));
  const keep = path.join(dir, "keep.png");
  const orphan = path.join(dir, "orphan.png");
  fs.writeFileSync(keep, "k");
  fs.writeFileSync(orphan, "o");
  const swept = sweepOutboundStaging(dir, [keep]);
  assert.equal(swept, 1);
  assert.ok(fs.existsSync(keep));
  assert.ok(!fs.existsSync(orphan));
  // Nonexistent dir is a no-op, not an error.
  assert.equal(sweepOutboundStaging(path.join(dir, "absent"), []), 0);
});
