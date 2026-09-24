// End-to-end harness for the brain-dump pipeline (ADR-005 / ADR-005a) with
// the two Claude sessions replaced by recorded replies. Exercises every
// step the bot runs after the model answers — parse the plan (prose + fence
// tolerated), persist it, render the rundown, interpret the operator's
// reply, apply into a temp vault — plus the unparseable-reply fallback. No
// session is spawned, no message is sent, and the real vault is not touched.
// All notes and names are synthetic.

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import {
  applyPlan,
  buildFallbackPlan,
  formatRundown,
  parseInterpretResponse,
  safeParsePlan,
} from "./braindump.js";
import { ChatSessionStore, PendingPlansStore, shortPlanId } from "./db.js";
import type { Config } from "./config.js";

const BUCKETS = ["0-Inbox", "2-Daily-Notes", "3-Projects/Alpha", "6-Slipbox"];

function tmpVault(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "braindump-e2e-"));
  for (const b of BUCKETS) fs.mkdirSync(path.join(dir, b), { recursive: true });
  fs.writeFileSync(
    path.join(dir, "3-Projects/Alpha/engine.md"),
    "---\ncreated: 2026-01-01\nsource: manual\n---\n# Engine\n\nExisting notes.\n",
  );
  return dir;
}

/** A planner reply as the model tends to produce it: prose, then a fence. */
const PLANNER_REPLY = `Ack posted. Here is the plan:

\`\`\`json
{
  "ops": [
    {
      "op": "append",
      "targetPath": "3-Projects/Alpha/engine.md",
      "body": "## Turbopump\\n\\nThe test stand is booked for Friday.",
      "reason": "the engine note already covers this theme"
    },
    {
      "op": "create",
      "bucket": "6-Slipbox",
      "filename": "test-early.md",
      "body": "---\\ncreated: 2026-01-02\\nsource: whatsapp-braindump\\ntags: [testing]\\n---\\n\\n# Test early\\n\\nSee [[engine]].\\n",
      "createsSubfolder": false,
      "reason": "atomic idea"
    }
  ],
  "summary": "appended to Alpha/engine, 1 slipbox idea",
  "confidence": 0.9
}
\`\`\``;

test("brain-dump pipeline: plan → persist → rundown → interpret → apply (temp vault)", () => {
  const vault = tmpVault();
  const dbPath = path.join(vault, ".plans.db");
  new ChatSessionStore(dbPath); // creates pending_plans
  const store = new PendingPlansStore(dbPath);
  const config = { vaultPath: vault } as unknown as Config;

  const plan = safeParsePlan(PLANNER_REPLY);
  assert.ok(plan, "planner reply must parse");
  assert.equal(plan.ops.length, 2);

  const planId = store.insert({
    chatId: "test@chat",
    captureText: "turbopump test stand friday; idea: test early",
    inputKind: "text",
    opsJson: JSON.stringify(plan.ops),
    summary: plan.summary,
    confidence: plan.confidence,
  });

  const rundown = formatRundown({
    planId,
    shortId: shortPlanId(planId),
    summary: plan.summary,
    confidence: plan.confidence,
    ops: plan.ops.map((op, i) => ({ id: i + 1, op })),
    elapsedMs: 0,
  });
  assert.match(rundown, /1\. /);
  assert.match(rundown, /2\. /);

  // Operator answers "sim" → interpreter returns apply-all.
  const verdict = parseInterpretResponse('{"action":"apply","ids":[1,2]}', plan.ops.length);
  assert.equal(verdict.action, "apply");

  const outcome = applyPlan(planId, verdict.ids ?? [], store, config);
  assert.deepEqual(outcome.ops.map((o) => o.status), ["ok", "ok"]);

  const engine = fs.readFileSync(path.join(vault, "3-Projects/Alpha/engine.md"), "utf8");
  assert.ok(engine.startsWith("---\ncreated: 2026-01-01"), "frontmatter kept");
  assert.match(engine, /Turbopump/);
  assert.ok(fs.existsSync(path.join(vault, "6-Slipbox/test-early.md")));
  assert.equal(store.get(planId)?.status, "applied");

  fs.rmSync(vault, { recursive: true, force: true });
});

test("brain-dump pipeline: unparseable planner reply falls back to 0-Inbox", () => {
  assert.equal(safeParsePlan("Plan emitted. **Decomposition:** two notes."), null);
  const fb = buildFallbackPlan("raw capture text", "voice", "2026-01-03", "prose", "");
  assert.equal(fb.ops.length, 1);
  const op = fb.ops[0];
  assert.equal(op.op, "create");
  if (op.op === "create") {
    assert.equal(op.bucket, "0-Inbox");
    assert.match(op.body, /raw capture text/);
  }
});

test("brain-dump pipeline: a malformed interpreter reply asks again", () => {
  assert.equal(parseInterpretResponse("sure, go ahead", 2).action, "ambiguous");
  assert.equal(parseInterpretResponse('{"action":"apply","ids":[9]}', 2).action, "ambiguous");
});
