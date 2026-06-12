// Outbound media enqueue CLI (ADR-018) — queues a file for WhatsApp
// delivery via the drain. Never opens Baileys (would knock the bot off).
//
// Two modes:
//
//   --doc <id-or-prefix> [--caption "…"]
//     Document-library retrieval — THE identity-document path. Target is
//     ALWAYS the operator's own DM, derived inside this CLI from the first
//     WHATSAPP_ALLOWED_DM_JIDS entry. No target flag is parsed in this
//     mode: a wrong destination is inexpressible here (the drain's
//     allowlist re-validates as the second layer, the skill's hard rule is
//     the third). Bumps retrieve_count + audit.
//
//   --path /abs/file --kind image|document --target <digits|jid|group-name>
//     Generic producer path (gallery etc.). Target validated by the drain.
//
// Both modes COPY the file into memory/outbound-staging/ — the queue row's
// media_path is drain-owned and unlinked at terminal state; pointing it at
// a library original would let the drain delete your passport.

import { randomUUID } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import { loadConfig } from "./config.js";
import { ChatSessionStore, OutboundQueueStore, type OutboundKind } from "./db.js";
import { DocStore } from "./docstore.js";

function fail(msg: string): never {
  console.error(JSON.stringify({ error: msg }));
  process.exit(2);
}

function parseArgs(argv: string[]): Map<string, string> {
  const flags = new Map<string, string>();
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    if (!a.startsWith("--")) fail(`unexpected positional argument: ${a}`);
    const v = argv[i + 1];
    if (v === undefined || v.startsWith("--")) fail(`flag ${a} needs a value`);
    flags.set(a.slice(2), v);
    i++;
  }
  return flags;
}

function stage(stagingDir: string, srcPath: string, ext: string): string {
  fs.mkdirSync(stagingDir, { recursive: true });
  const staged = path.join(stagingDir, `${randomUUID()}.${ext}`);
  fs.copyFileSync(srcPath, staged);
  return staged;
}

function main(): void {
  const workspaceRoot =
    process.env.NUCLEUS_WORKSPACE_ROOT ??
    path.resolve(import.meta.dirname, "..", "..", "..");
  const config = loadConfig(workspaceRoot, false);
  const flags = parseArgs(process.argv.slice(2));

  // Schema owner first (ack.ts convention), then the queue writer.
  new ChatSessionStore(config.dbPath);
  const queue = new OutboundQueueStore(config.dbPath);

  let target: string;
  let srcPath: string;
  let kind: OutboundKind;
  let mimetype: string | undefined;
  let filename: string | undefined;
  const caption = flags.get("caption") ?? "";

  if (flags.has("doc")) {
    // ── Document-library mode: DM-locked by construction ──
    if (flags.has("target")) {
      fail(
        "--doc mode takes no --target: identity documents go only to the " +
          "operator's own DM (derived from WHATSAPP_ALLOWED_DM_JIDS)",
      );
    }
    if (flags.has("path") || flags.has("kind")) {
      fail("--doc mode infers path/kind from the library; drop --path/--kind");
    }
    const dmDigits = [...config.allowedDmSenders][0];
    if (!dmDigits) fail("WHATSAPP_ALLOWED_DM_JIDS is empty — no operator DM to deliver to");
    target = dmDigits;

    const docStore = new DocStore({
      dbPath: config.documentsDbPath,
      documentsDir: config.documentsDir,
    });
    const doc = docStore.get(flags.get("doc")!);
    if (!doc) fail(`no active document matching ${flags.get("doc")}`);
    srcPath = docStore.pathFor(doc);
    if (!fs.existsSync(srcPath)) {
      fail(`library integrity problem: "${doc.logicalName}" indexed but file missing at ${srcPath}`);
    }
    kind = doc.mimetype.startsWith("image/") ? "image" : "document";
    mimetype = doc.mimetype;
    filename = doc.filename;
    docStore.recordRetrieval(doc.id, "whatsapp-dm");
  } else if (flags.has("path")) {
    // ── Generic producer mode ──
    srcPath = flags.get("path")!;
    const rawKind = flags.get("kind");
    const rawTarget = flags.get("target");
    if (!rawKind || (rawKind !== "image" && rawKind !== "document")) {
      fail("--path mode needs --kind image|document");
    }
    if (!rawTarget) fail("--path mode needs --target (digits, JID, or group name)");
    if (!fs.existsSync(srcPath)) fail(`file not found: ${srcPath}`);
    kind = rawKind;
    target = rawTarget;
    mimetype = flags.get("mimetype");
    filename = flags.get("filename") ?? path.basename(srcPath);
  } else {
    fail("usage: enqueue-media --doc <id> [--caption …] | --path <file> --kind image|document --target <t> [--caption …]");
  }

  const size = fs.statSync(srcPath).size;
  if (size > config.mediaMaxBytes) {
    fail(`file too large: ${size} bytes > ${config.mediaMaxBytes} cap (WHATSAPP_MEDIA_MAX_BYTES)`);
  }

  const ext = path.extname(srcPath).replace(".", "").toLowerCase() || "bin";
  const staged = stage(config.outboundStagingDir, srcPath, ext);
  const id = queue.enqueue({
    target,
    body: caption,
    source: "enqueue-media",
    kind,
    mediaPath: staged,
    mimetype,
    filename,
  });

  // Stale-queue heads-up: the bot drains every 1s, so pending rows older
  // than 60s mean it's down — the session relaying this can tell the
  // operator the file will arrive late.
  const stale = queue
    .pending(50)
    .filter((r) => r.id !== id && Date.now() - Date.parse(r.enqueuedAt) > 60_000);
  if (stale.length > 0) {
    console.error(
      JSON.stringify({
        warning: `outbound queue has ${stale.length} undrained row(s) older than 60s — the WhatsApp bot looks down; delivery will happen when it restarts`,
      }),
    );
  }

  console.log(JSON.stringify({ queued: id, target, kind, staged }));
}

main();
