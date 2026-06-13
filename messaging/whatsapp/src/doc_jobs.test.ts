import { test } from "node:test";
import assert from "node:assert/strict";
import { parseEnrichReply, parseImportReply } from "./doc_jobs.js";

test("parseEnrichReply: clean JSON, fenced JSON, unsupported, garbage", () => {
  const ok = parseEnrichReply('{"keywords": ["Lease", "Contrato "], "summary": "A lease.", "language": "en"}');
  assert.equal(ok.status, "ok");
  assert.deepEqual(ok.keywords, ["lease", "contrato"]);
  assert.equal(ok.summary, "A lease.");

  const fenced = parseEnrichReply('```json\n{"keywords":["a"],"summary":"s"}\n```');
  assert.equal(fenced.status, "ok");

  const unsupported = parseEnrichReply('{"unsupported": "docx not renderable"}');
  assert.equal(unsupported.status, "unsupported");

  const garbage = parseEnrichReply("I could not read the file, sorry!");
  assert.equal(garbage.status, "failed");

  const empty = parseEnrichReply('{"keywords": [], "summary": ""}');
  assert.equal(empty.status, "failed");
});

test("parseImportReply: validates slug shape and required fields", () => {
  const ok = parseImportReply('{"slug": "lease-2026", "title": "Lease", "markdown": "# Lease\\n\\nbody"}');
  assert.ok(!("error" in ok));

  const badSlug = parseImportReply('{"slug": "Bad Slug!", "title": "T", "markdown": "m"}');
  assert.ok("error" in badSlug && badSlug.error.includes("bad slug"));

  const missing = parseImportReply('{"slug": "ok-slug", "title": "", "markdown": "m"}');
  assert.ok("error" in missing);

  const garbage = parseImportReply("not json at all");
  assert.ok("error" in garbage);
});
