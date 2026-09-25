// Issue-pipeline surface on WhatsApp (ADR-036): group requests, the group
// budget, routing operator messages to items, and the intake groups in the
// target allowlist.

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { ChatSessionStore, OutboundQueueStore } from "./db.js";
import { parseToml } from "./config.js";
import {
  GroupExecutor,
  groupBudgetAllows,
  intakeConfig,
  IntakeStore,
  routeDm,
  stripGroupMarker,
  type GroupApi,
} from "./intake.js";
import { GroupAllowlist, resolveTarget, type TargetConfig } from "./target_policy.js";
import { DatabaseSync } from "node:sqlite";

// Synthetic identifiers built at runtime (the committed-secrets scanner
// reads literal JIDs and phone numbers as real identifiers).
const OP = ["55119", "99999999"].join("");
const GROUP = ["120363000000000009", "g.us"].join("@");

function tmpDb(): string {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "intake-test-"));
  const db = path.join(dir, "whatsapp.db");
  new ChatSessionStore(db); // creates outbound_queue
  return db;
}

/** A pipeline request as Rust inserts it (nucleus_core::whatsapp_queue). */
function request(db: string, item: string, action: "create" | "close", subject: string | null, enqueuedAt = new Date().toISOString()): void {
  new DatabaseSync(db)
    .prepare(
      `INSERT INTO intake_group_requests (item_key, action, subject, enqueued_at, status, dedup_key)
       VALUES (?, ?, ?, ?, 'pending', ?)`,
    )
    .run(item, action, subject, enqueuedAt, `intake:${item}:${action}`);
}

const quiet = { info: () => {}, warn: () => {} };

function fakeApi() {
  const calls: string[] = [];
  let n = 0;
  const api: GroupApi = {
    create: async (subject, participants) => {
      calls.push(`create ${subject} ${participants.join(",")}`);
      n += 1;
      return { jid: `12036300000000000${n}@g.us`, members: [...participants, "bot"] };
    },
    leave: async (jid) => {
      calls.push(`leave ${jid}`);
    },
  };
  return { api, calls };
}

test("the group budget counts creations in the last 24 hours", () => {
  const now = Date.parse("2026-09-24T12:00:00Z");
  const recent = "2026-09-24T09:00:00.000Z";
  const old = "2026-09-23T11:00:00.000Z";
  assert.equal(groupBudgetAllows([], now, 3), true);
  assert.equal(groupBudgetAllows([recent, recent, old, old], now, 3), true);
  assert.equal(groupBudgetAllows([recent, recent, recent], now, 3), false);
  assert.equal(groupBudgetAllows([], now, 0), false, "0 disables group creation");
});

test("DM messages go to an item by its marker or by a quoted pipeline message", () => {
  const has = (n: string) => n === "3";
  assert.deepEqual(routeDm("#3 approve", null, has), { item: "3", text: "approve" });
  assert.deepEqual(routeDm("  #3   use option B", null, has), { item: "3", text: "use option B" });
  assert.equal(routeDm("#4 approve", null, has), null, "no DM thread for #4");
  assert.equal(routeDm("#1 priority today is the report", null, has), null, "an ordinary message keeps its #");
  assert.equal(routeDm("hello", null, has), null);
  assert.deepEqual(routeDm("approve", "7", has), { item: "7", text: "approve" });
  assert.deepEqual(routeDm("#7 approve comment", "7", has), { item: "7", text: "approve comment" });
  assert.equal(stripGroupMarker("#5 approve", "5"), "approve");
  assert.equal(stripGroupMarker("#6 approve", "5"), "#6 approve");
});

test("operator messages are stored once and quoted messages map to their item", () => {
  const db = tmpDb();
  const store = new IntakeStore(db);
  assert.equal(store.recordInbound({ itemKey: "2", chatId: GROUP, waMsgId: "m1", text: "hi" }), true);
  assert.equal(store.recordInbound({ itemKey: "2", chatId: GROUP, waMsgId: "m1", text: "hi" }), false);
  const out = new OutboundQueueStore(db);
  const id = out.enqueue({ target: "dm", body: "[#2] plan", source: "intake:2", dedupKey: "intake:2:m1" });
  out.markInFlight(id, "WAMSG1");
  assert.equal(store.itemForSentMessage("WAMSG1"), "2");
  assert.equal(store.itemForSentMessage("OTHER"), null);
  assert.equal(store.hasDmThread("2"), true);
  assert.equal(store.hasDmThread("3"), false);
  assert.equal(store.unsentTo("dm"), 1);
});

test("the executor creates a group with the operator only, within the budget", async () => {
  const db = tmpDb();
  const store = new IntakeStore(db);
  const { api, calls } = fakeApi();
  const active: string[] = [];
  const ex = new GroupExecutor({
    store,
    api,
    config: intakeConfig({ max_groups_per_day: 1 }),
    operatorJid: () => `${OP}@s.whatsapp.net`,
    onActive: (j) => active.push(j),
    onClosed: () => {},
    log: quiet,
  });
  request(db, "1", "create", "#1 Fix the typo");
  request(db, "2", "create", "#2 Another");
  await ex.tick();
  assert.deepEqual(calls, [`create #1 Fix the typo ${OP}@s.whatsapp.net`]);
  assert.equal(store.group("1")?.status, "active");
  assert.equal(active.length, 1);
  assert.equal(store.itemForGroup(active[0]), "1");
  const second = store.group("2");
  assert.equal(second?.status, "fallback");
  assert.match(second?.reason ?? "", /limit of 1 new groups/);
  assert.equal(store.pendingRequests().length, 0);
  // A repeated request for an item with a group creates nothing.
  new DatabaseSync(db).prepare(`UPDATE intake_group_requests SET status = 'pending'`).run();
  await ex.tick();
  assert.equal(calls.length, 1);
});

test("a failed creation falls back to the DM and is not retried", async () => {
  const db = tmpDb();
  const store = new IntakeStore(db);
  const api: GroupApi = {
    create: async () => {
      throw new Error("rate-overlimit");
    },
    leave: async () => {},
  };
  const ex = new GroupExecutor({
    store,
    api,
    config: intakeConfig({}),
    operatorJid: () => `${OP}@s.whatsapp.net`,
    onActive: () => {},
    onClosed: () => {},
    log: quiet,
  });
  request(db, "4", "create", "#4 x");
  await ex.tick();
  assert.equal(store.group("4")?.status, "fallback");
  assert.match(store.group("4")?.reason ?? "", /rate-overlimit/);
  assert.equal(store.pendingRequests().length, 0);
});

test("a group is left only after its last messages went out", async () => {
  const db = tmpDb();
  const store = new IntakeStore(db);
  const { api, calls } = fakeApi();
  const closed: string[] = [];
  const ex = new GroupExecutor({
    store,
    api,
    config: intakeConfig({}),
    operatorJid: () => `${OP}@s.whatsapp.net`,
    onActive: () => {},
    onClosed: (j) => closed.push(j),
    log: quiet,
  });
  request(db, "5", "create", "#5 y");
  await ex.tick();
  const jid = store.group("5")!.jid!;
  const out = new OutboundQueueStore(db);
  const row = out.enqueue({ target: jid, body: "Item #5 is closed.", source: "intake:5" });
  request(db, "5", "close", null);
  await ex.tick();
  assert.equal(store.group("5")?.status, "active", "waits for the unsent message");
  out.markSent(row, "WAMSG");
  await ex.tick();
  assert.equal(store.group("5")?.status, "closed");
  assert.deepEqual(closed, [jid]);
  assert.equal(calls.at(-1), `leave ${jid}`);
});

test("intake groups are sendable while active and not after", () => {
  const config: TargetConfig = {
    allowedDmSenders: new Set([OP]),
    allowedChatIds: [],
    brainDumpChatIds: [],
    allowedGroupNames: [],
    brainDumpGroupNames: [],
  };
  const groups = new GroupAllowlist(config);
  assert.equal(resolveTarget(GROUP, config, groups), null);
  groups.addIntake([GROUP, `${OP}@s.whatsapp.net`]);
  assert.equal(resolveTarget(GROUP, config, groups), GROUP);
  assert.equal(groups.roles.get(`${OP}@s.whatsapp.net`), undefined, "only group JIDs");
  groups.removeIntake(GROUP);
  assert.equal(resolveTarget(GROUP, config, groups), null);
  // A configured group keeps its role.
  const configured = new GroupAllowlist({ ...config, allowedChatIds: [GROUP] }).addIntake([GROUP]);
  configured.removeIntake(GROUP);
  assert.equal(configured.roles.get(GROUP), "whatsapp-group");
});

test("the TOML reader keeps arrays of tables apart from the next table", () => {
  const t = parseToml(`
[intake]
enabled = true

[[intake.repos]]
repo = "owner/one"
test_command = "npm test"

[[intake.repos]]
repo = "owner/two"

[intake.whatsapp]
max_groups_per_day = 2
refinement_groups = false
`);
  assert.equal(t.intake.enabled, true);
  assert.deepEqual(
    t.intake.repos.map((r: any) => r.repo),
    ["owner/one", "owner/two"],
  );
  assert.equal(t.intake.repos[0].test_command, "npm test");
  assert.deepEqual(intakeConfig(t.intake.whatsapp), { refinementGroups: false, maxGroupsPerDay: 2 });
  assert.deepEqual(intakeConfig(undefined), { refinementGroups: true, maxGroupsPerDay: 3 });
});
