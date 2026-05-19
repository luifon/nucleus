// One-shot ack-sender for the brain-dump planning Claude session.
//
// ADR-005a: when the planning Claude session spawns, its first action is
// to call this script. The script writes a row to outbound_queue; the
// bot's drainer picks it up within ~1s and emits it via Baileys. The
// point is to give the operator a "Claude is alive and starting" signal
// that crosses the process boundary visibly.
//
// Usage (called by Claude via Bash from the workspace root):
//   npx --prefix messaging/whatsapp tsx messaging/whatsapp/src/ack.ts "<body>"
//
// Target is implicit — the first entry of WHATSAPP_BRAINDUMP_GROUP_NAMES.
// The bot's drainer is the only authorized writer to WhatsApp; this
// script never opens Baileys directly (a second Baileys connection from
// the same paired device would knock the bot off, see send.ts).

import path from "node:path";
import { loadConfig } from "./config.js";
import { ChatSessionStore, OutboundQueueStore } from "./db.js";

function formatReply(body: string, personaName: string): string {
  // Match index.ts formatReply: WhatsApp bold uses single asterisks and
  // doesn't cross newlines reliably, so wrap each non-empty line.
  const bolded = body
    .split("\n")
    .map((line) => (line.trim() ? `*${line}*` : line))
    .join("\n");
  return `${bolded}\n\n*— ${personaName}*`;
}

function main(): void {
  const body = process.argv.slice(2).join(" ").trim();
  if (!body) {
    console.error("usage: ack <message body>");
    process.exit(2);
  }

  const workspaceRoot =
    process.env.NUCLEUS_WORKSPACE_ROOT ??
    path.resolve(import.meta.dirname, "..", "..", "..");
  // loadConfig loads .env into process.env as a side effect AND resolves
  // the persona (ADR-009) — must happen before reading the display name,
  // otherwise the fallback fires whenever this script is invoked from a
  // context (tmux, npx) where .env hasn't already been sourced.
  const config = loadConfig(workspaceRoot, false);
  const personaName = config.personaDisplayName;

  const target = config.brainDumpGroupNames[0];
  if (!target) {
    console.error(
      "ack: WHATSAPP_BRAINDUMP_GROUP_NAMES is empty; nowhere to send",
    );
    process.exit(2);
  }

  // ChatSessionStore creates the schema (outbound_queue + pending_plans);
  // ensure it runs once before opening the queue writer.
  new ChatSessionStore(config.dbPath);
  const queue = new OutboundQueueStore(config.dbPath);
  const id = queue.enqueue({
    target,
    body: formatReply(body, personaName),
    source: "braindump-ack",
  });

  console.log(`ack queued (id=${id}, target=${target})`);
}

main();
