import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { DocStore, extFor, storedName, type ManifestEvent } from "./docstore.js";

function freshStore(onManifestChange?: (ev: ManifestEvent) => void): {
  store: DocStore;
  dir: string;
} {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-docstore-"));
  const store = new DocStore({
    dbPath: path.join(dir, "documents.db"),
    documentsDir: path.join(dir, "documents"),
    onManifestChange,
  });
  return { store, dir };
}

test("extFor: filename ext wins, mimetype map second, bin last", () => {
  assert.equal(extFor("rg.PDF", "application/pdf"), "pdf");
  assert.equal(extFor("photo", "image/jpeg"), "jpg");
  assert.equal(extFor("blob", "application/x-unknown"), "bin");
  assert.equal(storedName("abc", "pdf"), "abc.pdf");
});

test("add writes the file atomically and round-trips metadata", () => {
  const { store } = freshStore();
  const { record, deduped } = store.add({
    data: Buffer.from("hello docs"),
    logicalName: "Test Doc",
    tags: ["test", "demo"],
    filename: "test.txt",
    mimetype: "text/plain",
    source: "cli",
    channel: "cli",
  });
  assert.equal(deduped, false);
  assert.ok(fs.existsSync(store.pathFor(record)));
  assert.equal(fs.readFileSync(store.pathFor(record), "utf-8"), "hello docs");
  const got = store.get(record.id)!;
  assert.equal(got.logicalName, "Test Doc");
  assert.deepEqual(got.tags, ["test", "demo"]);
  assert.equal(got.bytes, 10);
  // No .tmp leftovers.
  const leftovers = fs
    .readdirSync(path.dirname(store.pathFor(record)))
    .filter((n) => n.startsWith(".tmp-"));
  assert.equal(leftovers.length, 0);
});

test("sha256 dedup short-circuits the second add", () => {
  const { store } = freshStore();
  const a = store.add({
    data: Buffer.from("same bytes"),
    logicalName: "First",
    filename: "a.txt",
    mimetype: "text/plain",
    source: "cli",
    channel: "cli",
  });
  const b = store.add({
    data: Buffer.from("same bytes"),
    logicalName: "Second name ignored",
    filename: "b.txt",
    mimetype: "text/plain",
    source: "cli",
    channel: "cli",
  });
  assert.equal(b.deduped, true);
  assert.equal(b.record.id, a.record.id);
  assert.equal(store.list().length, 1);
});

test("find tiers: exact name beats tag beats substring beats fuzzy", () => {
  const { store } = freshStore();
  const mk = (name: string, tags: string[] = []) =>
    store.add({
      data: Buffer.from(name),
      logicalName: name,
      tags,
      filename: `${name}.txt`,
      mimetype: "text/plain",
      source: "cli",
      channel: "cli",
    }).record;
  mk("passport", ["identity"]);
  mk("passport photo old", ["identity"]);
  mk("driving license", ["identity", "br"]);

  // Exact name → only the exact one, even though substring would match two.
  const exact = store.find("passport");
  assert.equal(exact.length, 1);
  assert.equal(exact[0].logicalName, "passport");

  // Tag tier when no name matches.
  const byTag = store.find("identity");
  assert.equal(byTag.length, 3);

  // Substring tier.
  const sub = store.find("photo");
  assert.equal(sub.length, 1);
  assert.equal(sub[0].logicalName, "passport photo old");

  // Fuzzy token overlap.
  const fuzzy = store.find("license driving");
  assert.equal(fuzzy[0].logicalName, "driving license");
});

test("get accepts unambiguous id prefixes only", () => {
  const { store } = freshStore();
  const { record } = store.add({
    data: Buffer.from("x"),
    logicalName: "X",
    filename: "x.txt",
    mimetype: "text/plain",
    source: "cli",
    channel: "cli",
  });
  assert.equal(store.get(record.id.slice(0, 8))?.id, record.id);
  assert.equal(store.get("zz"), null); // too short
});

test("rename/retag/retrieval audit + manifest hook fire; throwing hook is harmless", () => {
  const events: ManifestEvent[] = [];
  let throwOnce = true;
  const { store } = freshStore((ev) => {
    events.push(ev);
    if (throwOnce) {
      throwOnce = false;
      throw new Error("hook boom");
    }
  });
  const { record } = store.add({
    data: Buffer.from("y"),
    logicalName: "Y",
    filename: "y.txt",
    mimetype: "text/plain",
    source: "cli",
    channel: "cli",
  }); // hook threw here — op must still have succeeded
  assert.ok(store.get(record.id));

  store.rename(record.id, "Y2", "cli");
  store.retag(record.id, ["new"], "cli");
  store.recordRetrieval(record.id, "whatsapp-dm");

  assert.deepEqual(
    events.map((e) => e.action),
    ["store", "rename", "retag", "retrieve"],
  );
  const got = store.get(record.id)!;
  assert.equal(got.logicalName, "Y2");
  assert.equal(got.retrieveCount, 1);
  assert.ok(got.lastRetrievedAt);
  const audit = store.auditRows();
  assert.deepEqual(
    audit.map((a) => a.action).sort(),
    ["rename", "retag", "retrieve", "store"],
  );
});

test("remove soft-deletes, unlinks, and frees the sha for re-add", () => {
  const { store } = freshStore();
  const { record } = store.add({
    data: Buffer.from("z"),
    logicalName: "Z",
    filename: "z.txt",
    mimetype: "text/plain",
    source: "cli",
    channel: "cli",
  });
  const p = store.pathFor(record);
  store.remove(record.id, "cli");
  assert.equal(store.get(record.id), null);
  assert.ok(!fs.existsSync(p));
  const again = store.add({
    data: Buffer.from("z"),
    logicalName: "Z again",
    filename: "z.txt",
    mimetype: "text/plain",
    source: "cli",
    channel: "cli",
  });
  assert.equal(again.deduped, false, "deleted rows must not block re-add");
});

test("setEnrichment round-trips and find prefers tags over keywords over summary", () => {
  const { store } = freshStore();
  const tagged = store.add({
    data: Buffer.from("tagged doc"),
    logicalName: "Tagged",
    tags: ["lease"],
    filename: "t.txt",
    mimetype: "text/plain",
    source: "cli",
    channel: "cli",
  }).record;
  const keyworded = store.add({
    data: Buffer.from("keyworded doc"),
    logicalName: "Keyworded",
    filename: "k.txt",
    mimetype: "text/plain",
    source: "cli",
    channel: "cli",
  }).record;
  const summarized = store.add({
    data: Buffer.from("summarized doc"),
    logicalName: "Summarized",
    filename: "s.txt",
    mimetype: "text/plain",
    source: "cli",
    channel: "cli",
  }).record;
  store.setEnrichment(keyworded.id, { keywords: ["lease", "contrato"], summary: null, status: "ok" }, "cli");
  store.setEnrichment(summarized.id, { keywords: ["other"], summary: "about a lease agreement", status: "ok" }, "cli");

  // Exact tag tier wins outright — keyword matches not consulted.
  const byTag = store.find("lease");
  assert.equal(byTag.length, 1);
  assert.equal(byTag[0].logicalName, "Tagged");

  // Exact keyword tier when no tag matches.
  const byKeyword = store.find("contrato");
  assert.equal(byKeyword.length, 1);
  assert.equal(byKeyword[0].logicalName, "Keyworded");

  // Summary substring is the weakest auto tier.
  const bySummary = store.find("agreement");
  assert.equal(bySummary.length, 1);
  assert.equal(bySummary[0].logicalName, "Summarized");

  // Round-trip fields.
  const got = store.get(keyworded.id)!;
  assert.deepEqual(got.keywords, ["lease", "contrato"]);
  assert.equal(got.enrichStatus, "ok");
  assert.ok(got.enrichedAt);

  void tagged;
});

test("recordImport stores the pointer and audits", () => {
  const { store } = freshStore();
  const { record } = store.add({
    data: Buffer.from("imp"),
    logicalName: "Imp",
    filename: "i.txt",
    mimetype: "text/plain",
    source: "cli",
    channel: "cli",
  });
  store.recordImport(record.id, "5-Resources/Imported/2026-06-13-imp.md", "cli");
  assert.equal(store.get(record.id)!.importedPath, "5-Resources/Imported/2026-06-13-imp.md");
  assert.ok(store.auditRows().some((a) => a.action === "import"));
});
