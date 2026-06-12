// Document-library CLI (ADR-018) — what Claude sessions call via Bash.
//
// Conventions (ack.ts lineage): loadConfig for env, never opens Baileys,
// stores ensure their own schema. Output is JSON LINES on stdout (one
// object per row) so a session can parse without prose-stripping; errors
// are {"error": "..."} on stderr + exit 2.
//
// Usage:
//   docs.ts add --file /abs/path --name "RG Jane Doe" [--tags id,br] [--mimetype application/pdf]
//   docs.ts find <free text query…>
//   docs.ts list [--tag id] [--limit 20]
//   docs.ts path <id-or-prefix>
//   docs.ts rename <id-or-prefix> --name "new name"
//   docs.ts retag <id-or-prefix> --tags a,b

import path from "node:path";
import fs from "node:fs";
import { loadConfig } from "./config.js";
import { DocStore, type DocRecord } from "./docstore.js";
import { makeVaultManifestHook } from "./docstore_vault.js";

function fail(msg: string): never {
  console.error(JSON.stringify({ error: msg }));
  process.exit(2);
}

/** Pull `--flag value` pairs out of argv; returns [flags, positionals]. */
function parseArgs(argv: string[]): [Map<string, string>, string[]] {
  const flags = new Map<string, string>();
  const positional: string[] = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (a.startsWith("--")) {
      const v = argv[i + 1];
      if (v === undefined || v.startsWith("--")) fail(`flag ${a} needs a value`);
      flags.set(a.slice(2), v);
      i++;
    } else {
      positional.push(a);
    }
  }
  return [flags, positional];
}

function emit(d: DocRecord, store: DocStore): void {
  console.log(
    JSON.stringify({
      id: d.id,
      logical_name: d.logicalName,
      tags: d.tags,
      filename: d.filename,
      mimetype: d.mimetype,
      bytes: d.bytes,
      added_at: d.addedAt,
      retrieve_count: d.retrieveCount,
      path: store.pathFor(d),
      exists: fs.existsSync(store.pathFor(d)),
    }),
  );
}

function guessMimetype(file: string): string {
  const ext = path.extname(file).replace(".", "").toLowerCase();
  const map: Record<string, string> = {
    jpg: "image/jpeg",
    jpeg: "image/jpeg",
    png: "image/png",
    webp: "image/webp",
    gif: "image/gif",
    pdf: "application/pdf",
    txt: "text/plain",
    zip: "application/zip",
    docx: "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
    xlsx: "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
  };
  return map[ext] ?? "application/octet-stream";
}

function main(): void {
  const [cmd, ...rest] = process.argv.slice(2);
  if (!cmd) fail("usage: docs.ts <add|find|list|path|rename|retag> …");

  const workspaceRoot =
    process.env.NUCLEUS_WORKSPACE_ROOT ??
    path.resolve(import.meta.dirname, "..", "..", "..");
  const config = loadConfig(workspaceRoot, false);
  const store = new DocStore({
    dbPath: config.documentsDbPath,
    documentsDir: config.documentsDir,
    onManifestChange: makeVaultManifestHook(config),
  });

  const [flags, positional] = parseArgs(rest);

  switch (cmd) {
    case "add": {
      const file = flags.get("file");
      const name = flags.get("name");
      if (!file || !name) fail("add needs --file and --name");
      if (!fs.existsSync(file)) fail(`file not found: ${file}`);
      const { record, deduped } = store.add({
        data: { path: file },
        logicalName: name,
        tags: flags.get("tags")?.split(",").map((t) => t.trim()).filter(Boolean),
        filename: path.basename(file),
        mimetype: flags.get("mimetype") ?? guessMimetype(file),
        source: "cli",
        channel: "cli",
      });
      console.log(JSON.stringify({ id: record.id, deduped, path: store.pathFor(record) }));
      break;
    }
    case "find": {
      const query = positional.join(" ").trim();
      if (!query) fail("find needs a query");
      const matches = store.find(query);
      if (matches.length === 0) {
        console.log(JSON.stringify({ matches: 0 }));
      } else {
        for (const d of matches) emit(d, store);
      }
      break;
    }
    case "list": {
      const docs = store.list({
        tag: flags.get("tag"),
        limit: flags.get("limit") ? Number(flags.get("limit")) : undefined,
      });
      for (const d of docs) emit(d, store);
      break;
    }
    case "path": {
      const id = positional[0];
      if (!id) fail("path needs an id");
      const d = store.get(id);
      if (!d) fail(`no active document matching ${id}`);
      console.log(JSON.stringify({ id: d.id, path: store.pathFor(d), exists: fs.existsSync(store.pathFor(d)) }));
      break;
    }
    case "rename": {
      const id = positional[0];
      const name = flags.get("name");
      if (!id || !name) fail("rename needs <id> and --name");
      const d = store.get(id);
      if (!d) fail(`no active document matching ${id}`);
      store.rename(d.id, name, "cli");
      console.log(JSON.stringify({ id: d.id, logical_name: name }));
      break;
    }
    case "retag": {
      const id = positional[0];
      const tags = flags.get("tags");
      if (!id || tags === undefined) fail("retag needs <id> and --tags");
      const d = store.get(id);
      if (!d) fail(`no active document matching ${id}`);
      const list = tags.split(",").map((t) => t.trim()).filter(Boolean);
      store.retag(d.id, list, "cli");
      console.log(JSON.stringify({ id: d.id, tags: list }));
      break;
    }
    default:
      fail(`unknown subcommand: ${cmd}`);
  }
}

main();
