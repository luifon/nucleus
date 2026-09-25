// Who may run the scripts that put a message on WhatsApp (ADR-033).
//
// send.ts, ack.ts and enqueue-media.ts send outside the turn engine, so they
// are open only to callers that have a reason to use them. The caller is
// decided from the process tree (proc_tree.ts), never from the script's own
// environment:
//
//   - The operator (a terminal, or a Claude Code session the operator
//     started) may run all three.
//   - A brain-dump planning session may send its acknowledgement (ack.ts).
//   - A WhatsApp DM chat session with a valid task scope may deliver a
//     library document to the operator's own DM (enqueue-media.ts --doc).
//   - Everything else is refused: background task workers, other chat and
//     Nucleus sessions, detached processes, and processes whose tree cannot
//     be read.
//
// The Rust CLIs (tasks, session-send) apply the same process-tree rules in
// nucleus_core::caller.

import { DatabaseSync } from "node:sqlite";
import { isWorker, origin as readOrigin, type Origin } from "./proc_tree.js";
import { sha256Hex } from "./turn_store.js";

export type SendAction = "send" | "ack" | "document";

/** Refusal reason, or null when `action` is allowed. Pure. */
export function refusal(action: SendAction, o: Origin, scopeValid: (token: string) => boolean): string | null {
  switch (o.origin) {
    case "terminal":
    case "operator-session":
      return null;
    case "detached":
      return "the caller has no terminal and no session (a detached process); run it from a terminal";
    case "unknown":
      return `the caller cannot be identified: ${o.reason}`;
    case "nucleus": {
      const n = o.session;
      if (isWorker(n)) {
        return "this is a background task worker session. Workers do not send messages; the task result is delivered automatically";
      }
      if (action === "ack" && n.kind === "braindump") return null;
      if (action === "document" && n.kind === "chat") {
        if (n.scope && scopeValid(n.scope)) return null;
        return "only the WhatsApp DM chat session (with a valid task scope) may deliver documents";
      }
      return `a Nucleus ${n.kind} session may not send WhatsApp messages directly; its reply is sent by the bot`;
    }
  }
}

/** True when `token` is a recorded, unrevoked task scope in whatsapp.db. */
export function scopeValidIn(dbPath: string): (token: string) => boolean {
  return (token) => {
    try {
      const db = new DatabaseSync(dbPath, { readOnly: true });
      try {
        const row = db.prepare(`SELECT 1 FROM task_scopes WHERE token_sha256 = ?`).get(sha256Hex(token));
        return row !== undefined;
      } finally {
        db.close();
      }
    } catch {
      return false;
    }
  };
}

/** Exit with status 3 unless the caller may perform `action`. */
export function refuseUnlessAllowed(what: string, action: SendAction, dbPath: string, o: Origin = readOrigin()): void {
  // The caller's own environment can only narrow what the tree allows.
  const envWorker = process.env.NUCLEUS_TASK_WORKER?.trim();
  const reason = envWorker
    ? `this is background task ${envWorker.slice(0, 8)}'s worker session. Workers do not send messages; the task result is delivered automatically`
    : refusal(action, o, scopeValidIn(dbPath));
  if (reason) {
    console.error(`${what}: refused — ${reason} (ADR-033).`);
    process.exit(3);
  }
}
