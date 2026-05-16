// One-shot operator tool: clear the membership-tripwire baselines after
// a bot-identity change (e.g., re-pairing under a new phone number).
// Next inbound message in each group records a fresh baseline from
// whatever WhatsApp currently reports as the participant set.

import { DatabaseSync } from "node:sqlite";
import path from "node:path";

const workspaceRoot =
  process.env.NUCLEUS_WORKSPACE_ROOT ??
  path.resolve(import.meta.dirname, "..", "..", "..");
const dbPath = path.join(workspaceRoot, "memory/whatsapp.db");

const db = new DatabaseSync(dbPath);
const before = db.prepare("SELECT chat_id, disabled FROM chat_state").all();
db.prepare("DELETE FROM chat_state").run();
console.log(JSON.stringify({ cleared: before.length, before }, null, 2));
db.close();
