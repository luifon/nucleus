// The target policy every sending path applies (ADR-033): only the
// operator's DM and the configured groups, whoever the caller is.

import { test } from "node:test";
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { normalizeSenderId as normalize } from "./config.js";
import {
  enqueueRefusal,
  GroupAllowlist,
  isOperatorOnly,
  pickOperatorDm,
  resolveQueuedTarget,
  resolveTarget,
  type TargetConfig,
} from "./target_policy.js";

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

test("operator-only messages reach only the operator; replies stay in the writer's chat", async () => {
  // The operator (phone OP, LID OP_LID) and another allowed contact (OTHER).
  const OP_LID = ["12345", "6789012345"].join("");
  const both: TargetConfig = { ...config, allowedDmSenders: new Set([OP, OTHER]), operatorLids: new Set([OP_LID]) };
  const groups = new GroupAllowlist(both);
  const opChat = `${OP_LID}@lid`;
  const otherChat = `${OTHER}@s.whatsapp.net`;
  const isOp = async (jid: string) => jid === opChat || jid.startsWith(`${OP}@`);
  const isOpSync = (c: string) => c === opChat || c.startsWith(`${OP}@`);
  // The other contact was active more recently: `dm` still picks the operator.
  const recency = [otherChat, opChat];
  assert.equal(pickOperatorDm(recency, isOpSync, OP), opChat);
  assert.equal(pickOperatorDm([otherChat], isOpSync, OP), `${OP}@s.whatsapp.net`, "falls back to the phone JID");
  const q = (target: string, source: string, operatorDm = () => pickOperatorDm(recency, isOpSync, OP), isOperator = isOp) =>
    resolveQueuedTarget({ target, source, config: both, groups, operatorDm, operatorPhone: OP, isOperator });
  assert.equal(await q("dm", "intake:ask"), opChat);
  // A pipeline message addressed to the other contact's chat is refused.
  assert.equal(await q(otherChat, "intake:3"), null);
  assert.equal(await q(otherChat, "reminders"), null);
  // A chat-engine reply to the other contact goes to that contact.
  assert.equal(await q(otherChat, "chat-reply"), otherChat);
  // A reminder to whatsapp-dm (the first allowlist entry) reaches the operator.
  assert.equal(await q(OP, "reminders"), `${OP}@s.whatsapp.net`);
  // A LID the live check no longer accepts: `dm` falls back to the phone.
  assert.equal(await q("dm", "intake:ask", () => opChat, async (j) => j.startsWith(`${OP}@`)), `${OP}@s.whatsapp.net`);
  // A task result for an operator LID chat not in the lists: delivered when
  // the live check accepts it, redirected to the operator's phone otherwise.
  const mappedChat = `${["22222", "3333344444"].join("")}@lid`;
  assert.equal(await q(mappedChat, "task:ab12cd34", undefined, async (j) => j === mappedChat), mappedChat);
  assert.equal(await q(mappedChat, "task:ab12cd34", undefined, async (j) => j.startsWith(`${OP}@`)), `${OP}@s.whatsapp.net`);
  // A reply to an unknown LID (not a task result, not operator-only) is refused.
  assert.equal(await q(mappedChat, "chat-reply", undefined, async (j) => j.startsWith(`${OP}@`)), null);
  // A task result for an operator LID the live mapping now rejects: the LID
  // loses its verification and the row goes to the phone, not dropped.
  const staleCalls: Array<[string, string]> = [];
  const stale = await resolveQueuedTarget({
    target: opChat,
    source: "task:ab12cd34",
    config: both,
    groups,
    operatorDm: () => null,
    operatorPhone: OP,
    isOperator: async (j) => j.startsWith(`${OP}@`),
    onStaleLid: (lid, to) => staleCalls.push([lid, to]),
  });
  assert.equal(stale, `${OP}@s.whatsapp.net`);
  assert.deepEqual(staleCalls, [[opChat, `${OP}@s.whatsapp.net`]]);
  // The same for an operator-only row.
  staleCalls.length = 0;
  const staleOnly = await resolveQueuedTarget({
    target: opChat,
    source: "intake:ask",
    config: both,
    groups,
    operatorDm: () => null,
    operatorPhone: OP,
    isOperator: async (j) => j.startsWith(`${OP}@`),
    onStaleLid: (lid, to) => staleCalls.push([lid, to]),
  });
  assert.equal(staleOnly, `${OP}@s.whatsapp.net`);
  assert.equal(staleCalls.length, 1);
  // Another allowed contact's LID keeps its task result.
  const otherLid = `${["44444", "5555566666"].join("")}@lid`;
  const withOtherLid: TargetConfig = { ...both, allowedDmSenders: new Set([OP, OTHER, normalize(otherLid)]) };
  staleCalls.length = 0;
  const kept = await resolveQueuedTarget({
    target: otherLid,
    source: "task:ab12cd34",
    config: withOtherLid,
    groups: new GroupAllowlist(withOtherLid),
    operatorDm: () => null,
    operatorPhone: OP,
    isOperator: async () => false,
    onStaleLid: (lid, to) => staleCalls.push([lid, to]),
  });
  assert.equal(kept, otherLid);
  assert.equal(staleCalls.length, 0);
  assert.equal(isOperatorOnly("dm", "chat-reply"), true);
  assert.equal(isOperatorOnly(otherChat, "chat-reply"), false);
});

test("an operator LID is a sendable DM; another LID is not", () => {
  const groups = new GroupAllowlist(config);
  const lid = ["12345", "6789012345"].join("");
  assert.equal(resolveTarget(`${lid}@lid`, config, groups), null);
  const withLid: TargetConfig = { ...config, operatorLids: new Set([lid]) };
  assert.equal(resolveTarget(`${lid}@lid`, withLid, groups), `${lid}@lid`);
  assert.equal(resolveTarget(`${lid}@s.whatsapp.net`, withLid, groups), null, "only in LID form");
});

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
