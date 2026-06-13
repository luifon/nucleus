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

import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { writeImportedNote } from "./doc_jobs.js";
import type { Config } from "./config.js";
import type { DocRecord } from "./docstore.js";

function fakeConfig(vaultPath: string): Config {
  return { vaultPath } as Config; // writeImportedNote reads only vaultPath
}

function fakeRecord(): DocRecord {
  return {
    id: "doc-uuid-1234",
    logicalName: "Lease 2026",
    tags: [],
    filename: "lease.pdf",
    ext: "pdf",
    mimetype: "application/pdf",
    bytes: 10,
    sha256: "abc123",
    source: "inbound-dm",
    addedAt: "2026-06-13T00:00:00Z",
    lastRetrievedAt: null,
    retrieveCount: 0,
    keywords: [],
    summary: null,
    enrichedAt: null,
    enrichStatus: null,
    importedPath: null,
  };
}

test("writeImportedNote writes frontmatter + body under 5-Resources/Imported", () => {
  const vault = fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-vault-"));
  const rel = writeImportedNote(fakeConfig(vault), fakeRecord(), {
    slug: "lease-2026",
    title: "Lease 2026",
    markdown: "# Terms\n\n- pets allowed",
  });
  assert.ok(rel.startsWith("5-Resources/Imported/"));
  assert.ok(rel.endsWith("-lease-2026.md"));
  const body = fs.readFileSync(path.join(vault, rel), "utf-8");
  assert.ok(body.includes("source: whatsapp-doc-import"));
  assert.ok(body.includes("source_doc_id: doc-uuid-1234"));
  assert.ok(body.includes("source_sha256: abc123"));
  assert.ok(body.includes("# Lease 2026"));
  assert.ok(body.includes("pets allowed"));
});

test("writeImportedNote suffixes on collision", () => {
  const vault = fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-vault-"));
  const first = writeImportedNote(fakeConfig(vault), fakeRecord(), {
    slug: "same-slug",
    title: "A",
    markdown: "a",
  });
  const second = writeImportedNote(fakeConfig(vault), fakeRecord(), {
    slug: "same-slug",
    title: "B",
    markdown: "b",
  });
  assert.notEqual(first, second);
  assert.ok(second.endsWith("-same-slug-2.md"));
  assert.ok(fs.existsSync(path.join(vault, first)));
  assert.ok(fs.existsSync(path.join(vault, second)));
});
