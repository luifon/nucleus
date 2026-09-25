// The target policy every sending path applies (ADR-033): only the
// operator's DM and the configured groups, whoever the caller is.

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { enqueueRefusal, GroupAllowlist, resolveTarget, type TargetConfig } from "./target_policy.js";

// Synthetic identifiers built at runtime: the committed-secrets scanner reads
// literal JIDs and phone numbers as real identifiers.
const OP = ["55119", "99999999"].join("");
const OTHER = ["55119", "88888888"].join("");
const GROUP = ["120363000000000001", "g.us"].join("@");
const FOREIGN_GROUP = ["120363000000000002", "g.us"].join("@");
const NAMED_GROUP = ["120363000000000003", "g.us"].join("@");

const config: TargetConfig = {
  allowedDmSenders: new Set([OP]),
  allowedChatIds: [GROUP],
  brainDumpChatIds: [],
  allowedGroupNames: [],
  brainDumpGroupNames: ["Capture Group"],
};

test("the drain and send.ts resolve only the operator's DM and configured groups", () => {
  const groups = new GroupAllowlist(config, [
    { jid: NAMED_GROUP, subject: "Capture Group" },
    { jid: FOREIGN_GROUP, subject: "Family" },
  ]);
  assert.equal(resolveTarget(OP, config, groups), `${OP}@s.whatsapp.net`);
  assert.equal(resolveTarget(`${OP}@s.whatsapp.net`, config, groups), `${OP}@s.whatsapp.net`);
  assert.equal(resolveTarget(`${OP}@lid`, config, groups), `${OP}@lid`);
  assert.equal(resolveTarget("dm", config, groups, () => `${OP}@s.whatsapp.net`), `${OP}@s.whatsapp.net`);
  assert.equal(resolveTarget(GROUP, config, groups), GROUP);
  assert.equal(resolveTarget(NAMED_GROUP, config, groups), NAMED_GROUP);
  assert.equal(resolveTarget("capture group", config, groups), NAMED_GROUP);

  assert.equal(resolveTarget(OTHER, config, groups), null);
  assert.equal(resolveTarget(`${OTHER}@s.whatsapp.net`, config, groups), null);
  assert.equal(resolveTarget(FOREIGN_GROUP, config, groups), null);
  assert.equal(resolveTarget("Family", config, groups), null);
  assert.equal(resolveTarget("dm", config, groups, () => `${OTHER}@s.whatsapp.net`), null);
  assert.equal(resolveTarget("", config, groups), null);
});

test("queue writers refuse any other target at enqueue time", () => {
  assert.equal(enqueueRefusal("dm", config), null);
  assert.equal(enqueueRefusal(OP, config), null);
  assert.equal(enqueueRefusal(GROUP, config), null);
  assert.equal(enqueueRefusal("capture group", config), null);
  assert.match(enqueueRefusal(OTHER, config)!, /not the operator's DM/);
  assert.match(enqueueRefusal(FOREIGN_GROUP, config)!, /not in WHATSAPP_ALLOWED_CHAT_IDS/);
  assert.match(enqueueRefusal("Family", config)!, /group name is not in/);
});

/** A workspace with the configuration the scripts load and no WhatsApp
 *  credentials: a script that got past its target check would fail to
 *  connect, never send. */
function workspace(): string {
  const ws = fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-target-"));
  fs.mkdirSync(path.join(ws, "memory"));
  fs.mkdirSync(path.join(ws, "messaging", "whatsapp"), { recursive: true });
  fs.mkdirSync(path.join(ws, "personas"));
  fs.writeFileSync(path.join(ws, "personas", "test.md"), "---\ndisplay_name: Test\n---\nbody\n");
  return ws;
}

function run(script: string, args: string[], ws: string) {
  const tsx = path.join(import.meta.dirname, "..", "node_modules", ".bin", "tsx");
  return spawnSync(tsx, [path.join(import.meta.dirname, script), ...args], {
    encoding: "utf8",
    timeout: 60_000,
    env: {
      ...process.env,
      NUCLEUS_WORKSPACE_ROOT: ws,
      NUCLEUS_USER_NAME: "Test Operator",
      NUCLEUS_PERSONA_WHATSAPP: "test",
      WHATSAPP_ALLOWED_DM_JIDS: OP,
      WHATSAPP_ALLOWED_CHAT_IDS: GROUP,
      WHATSAPP_BRAINDUMP_GROUP_NAMES: "Capture Group",
      WHATSAPP_ALLOWED_GROUP_NAMES: "",
      WHATSAPP_BRAINDUMP_CHAT_IDS: "",
    },
  });
}

test("send.ts refuses a number that is not the operator's DM before any connection, whoever runs it", () => {
  const ws = workspace();
  const r = run("send.ts", [OTHER, "hello"], ws);
  assert.equal(r.status, 3, r.stdout + r.stderr);
  assert.match(r.stderr, /not the operator's DM or a configured group/);
  assert.ok(!fs.existsSync(path.join(ws, "messaging", "whatsapp", "auth")), "no connection was attempted");
  const g = run("send.ts", ["Family", "hello"], ws);
  assert.equal(g.status, 3, g.stdout + g.stderr);
  fs.rmSync(ws, { recursive: true, force: true });
});

test("enqueue-media refuses a foreign target and queues nothing", () => {
  const ws = workspace();
  const file = path.join(ws, "f.txt");
  fs.writeFileSync(file, "x");
  const r = run("enqueue-media.ts", ["--path", file, "--kind", "document", "--target", OTHER], ws);
  assert.notEqual(r.status, 0, r.stdout + r.stderr);
  assert.match(r.stdout + r.stderr, /target .* refused: the number is not the operator's DM/);
  fs.rmSync(ws, { recursive: true, force: true });
});
