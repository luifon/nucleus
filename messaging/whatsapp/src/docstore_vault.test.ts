import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { DocStore } from "./docstore.js";
import { appendAudit, ensureOverview, rewriteManifest } from "./docstore_vault.js";

function setup(): { store: DocStore; dbPath: string; vaultDir: string } {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-vaulttest-"));
  const dbPath = path.join(root, "documents.db");
  const vaultDir = path.join(root, "vault", "4-Areas", "Documents");
  const store = new DocStore({
    dbPath,
    documentsDir: path.join(root, "documents"),
    onManifestChange: (ev) => {
      ensureOverview(vaultDir);
      appendAudit(vaultDir, ev);
      rewriteManifest(dbPath, vaultDir);
    },
  });
  return { store, dbPath, vaultDir };
}

test("manifest regenerates as a view; rename never leaves a stale section", () => {
  const { store, vaultDir } = setup();
  const { record } = store.add({
    data: Buffer.from("doc-a"),
    logicalName: "Old Name",
    tags: ["t1"],
    filename: "a.txt",
    mimetype: "text/plain",
    source: "cli",
    channel: "cli",
  });

  const manifestPath = path.join(vaultDir, "manifest.md");
  let manifest = fs.readFileSync(manifestPath, "utf-8");
  assert.ok(manifest.includes("## Old Name"));
  const created = manifest.match(/^created:\s*(\S+)/m)![1];

  store.rename(record.id, "New Name", "cli");
  manifest = fs.readFileSync(manifestPath, "utf-8");
  assert.ok(manifest.includes("## New Name"));
  assert.ok(!manifest.includes("## Old Name"), "stale section must vanish on regenerate");
  assert.equal(manifest.match(/^created:\s*(\S+)/m)![1], created, "created: preserved");
  assert.ok(manifest.includes("generated:"), "generated: stamp present");
  // No tmp leftover.
  assert.ok(!fs.existsSync(path.join(vaultDir, "manifest.md.tmp")));
});

test("audit.md is append-only with monthly headings and survives rewrites", () => {
  const { store, vaultDir } = setup();
  const { record } = store.add({
    data: Buffer.from("doc-b"),
    logicalName: "B",
    filename: "b.txt",
    mimetype: "text/plain",
    source: "cli",
    channel: "cli",
  });
  store.recordRetrieval(record.id, "whatsapp-dm");

  const audit = fs.readFileSync(path.join(vaultDir, "audit.md"), "utf-8");
  const month = new Date().toISOString().slice(0, 7);
  assert.equal(audit.split(`## ${month}`).length, 2, "one monthly heading");
  assert.ok(audit.includes("— store —"));
  assert.ok(audit.includes("— retrieve —"));
  assert.ok(audit.includes("whatsapp-dm"));
});

test("overview is written once and never overwritten", () => {
  const { store, vaultDir } = setup();
  store.add({
    data: Buffer.from("doc-c"),
    logicalName: "C",
    filename: "c.txt",
    mimetype: "text/plain",
    source: "cli",
    channel: "cli",
  });
  const overviewPath = path.join(vaultDir, "Documents-overview.md");
  fs.appendFileSync(overviewPath, "\nOPERATOR CURATION MARKER\n");
  store.add({
    data: Buffer.from("doc-d"),
    logicalName: "D",
    filename: "d.txt",
    mimetype: "text/plain",
    source: "cli",
    channel: "cli",
  });
  assert.ok(
    fs.readFileSync(overviewPath, "utf-8").includes("OPERATOR CURATION MARKER"),
    "ensureOverview must not clobber operator edits",
  );
});
