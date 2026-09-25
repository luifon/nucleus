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
  classifyCreateError,
  closeBackoffMs,
  newNonce,
  nonceSuffix,
  GroupExecutor,
  groupBudgetAllows,
  intakeConfig,
  IntakeStore,
  isOperatorId,
  MAX_CLOSE_ATTEMPTS,
  routeDm,
  routeOperatorDm,
  stripGroupMarker,
  unexpectedMembers,
  type GroupApi,
  type GroupExecutorDeps,
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
const BOT = ["55119", "88888888"].join("");
const WA = ["s", "whatsapp", "net"].join(".");
/** A synthetic group JID built at runtime. */
const gjid = (n: string) => ["1203630000000000" + n, "g.us"].join("@");
const OP_LID = ["1234567", "89012345"].join("");
const STRANGER = ["55119", "77777777"].join("");

/** Executor deps with the operator and the bot as the only known ids. */
function deps(store: IntakeStore, api: GroupApi, over: Partial<GroupExecutorDeps> = {}) {
  const alerts: string[] = [];
  const alertKeys: string[] = [];
  const seeded: Array<{ jid: string; members: string[]; reason: string | null }> = [];
  const d: GroupExecutorDeps = {
    store,
    api,
    config: intakeConfig({}),
    operatorJid: () => `${OP}@s.whatsapp.net`,
    isOperator: (jid) => isOperatorId(jid, OP, async (lid) => (lid === `${OP_LID}@lid` ? `${OP}@s.whatsapp.net` : null)),
    selfIds: () => [`${BOT}:3@${WA}`],
    seedMembers: (jid, members, reason) => seeded.push({ jid, members, reason }),
    alertOperator: (text, key) => {
      alerts.push(text);
      alertKeys.push(key);
    },
    onActive: () => {},
    onClosed: () => {},
    log: quiet,
    ...over,
  };
  return { d, alerts, alertKeys, seeded };
}

function fakeApi() {
  const calls: string[] = [];
  const groups: Array<{ jid: string; subject: string; members: string[] }> = [];
  let n = 0;
  const api: GroupApi = {
    create: async (subject, participants) => {
      calls.push(`create ${subject} ${participants.join(",")}`);
      n += 1;
      const g = { jid: `12036300000000000${n}@g.us`, subject, members: [...participants, `${BOT}@s.whatsapp.net`] };
      groups.push(g);
      return { jid: g.jid, members: g.members };
    },
    leave: async (jid) => {
      calls.push(`leave ${jid}`);
      const i = groups.findIndex((g) => g.jid === jid);
      if (i >= 0) groups.splice(i, 1);
    },
    listParticipating: async () => [...groups],
  };
  return { api, calls, groups };
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

test("#n release <code> reaches the item as a command only when the operator typed it", async () => {
  const has = (n: string) => n === "3";
  const noLid = async () => null;
  const base = { operatorId: OP, pnForLid: noLid, quotedItem: null, hasDmThread: has };
  const opChat = `${OP}@s.whatsapp.net`;
  // Typed by the operator: routed as text, which the pipeline acts on.
  assert.deepEqual(await routeOperatorDm({ ...base, chatId: opChat, text: "#3 release a1b2c3", inputKind: "text" }), {
    item: "3",
    text: "release a1b2c3",
    inputKind: "text",
  });
  // A transcribed voice note or a forwarded message keeps its kind; the
  // pipeline keeps it in the thread and does not act on it.
  assert.equal((await routeOperatorDm({ ...base, chatId: opChat, text: "#3 release a1b2c3", inputKind: "voice" }))?.inputKind, "voice");
  assert.equal((await routeOperatorDm({ ...base, chatId: opChat, text: "#3 release a1b2c3", inputKind: "forwarded" }))?.inputKind, "forwarded");
  // Another sender's "#3 release a1b2c3" never reaches the item.
  const other = `${["55119", "88888888"].join("")}@s.whatsapp.net`;
  assert.equal(await routeOperatorDm({ ...base, chatId: other, text: "#3 release a1b2c3", inputKind: "text" }), null);
  // The operator in LID form, resolved through the connection's mapping.
  const lid = ["123456789012345", "lid"].join("@");
  const viaLid = await routeOperatorDm({ ...base, chatId: lid, pnForLid: async () => opChat, text: "#3 release a1b2c3", inputKind: "text" });
  assert.equal(viaLid?.item, "3");
  // An unknown LID is not the operator.
  assert.equal(await routeOperatorDm({ ...base, chatId: lid, text: "#3 release a1b2c3", inputKind: "text" }), null);
});

test("operator messages are stored once and quoted messages map to their item", () => {
  const db = tmpDb();
  const store = new IntakeStore(db);
  assert.equal(store.recordInbound({ itemKey: "2", chatId: GROUP, waMsgId: "m1", text: "hi", inputKind: "voice" }), true);
  assert.equal(store.recordInbound({ itemKey: "2", chatId: GROUP, waMsgId: "m1", text: "hi", inputKind: "text" }), false);
  const kind = new DatabaseSync(db).prepare(`SELECT input_kind FROM intake_inbound WHERE wa_msg_id = 'm1'`).get() as { input_kind: string };
  assert.equal(kind.input_kind, "voice", "how the message was written is stored");
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
  const { d, seeded } = deps(store, api, { config: intakeConfig({ max_groups_per_day: 1 }), onActive: (j) => active.push(j) });
  const ex = new GroupExecutor(d);
  request(db, "1", "create", "#1 Fix the typo");
  request(db, "2", "create", "#2 Another");
  await ex.tick();
  assert.equal(calls.length, 1);
  assert.match(calls[0], new RegExp(`^create #1 Fix the typo ~[0-9a-f]{16} ${OP}@s\\.whatsapp\\.net$`), "the subject ends with a random nonce");
  assert.equal(store.group("1")?.status, "active");
  assert.equal(active.length, 1);
  assert.equal(store.itemForGroup(active[0]), "1");
  assert.deepEqual(seeded, [{ jid: active[0], members: [`${OP}@s.whatsapp.net`, `${BOT}@s.whatsapp.net`], reason: null }], "baseline from the create response");
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
      // A refusal from WhatsApp (a 4xx answer): nothing was created.
      throw Object.assign(new Error("rate-overlimit"), { output: { statusCode: 429 } });
    },
    leave: async () => {},
    listParticipating: async () => [],
  };
  const ex = new GroupExecutor(deps(store, api).d);
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
  const ex = new GroupExecutor(deps(store, api, { onClosed: (j) => closed.push(j) }).d);
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

test("only the operator's own identity counts, in phone or LID form", async () => {
  const pn = async (lid: string) => (lid === `${OP_LID}@lid` ? `${OP}@s.whatsapp.net` : null);
  assert.equal(await isOperatorId(`${OP}@s.whatsapp.net`, OP, pn), true);
  assert.equal(await isOperatorId(`${OP}:12@${WA}`, OP, pn), true, "any device");
  assert.equal(await isOperatorId(`${OP_LID}@lid`, OP, pn), true, "LID mapped to the operator's number");
  assert.equal(await isOperatorId(`${STRANGER}@s.whatsapp.net`, OP, pn), false);
  assert.equal(await isOperatorId(`${OP_LID}@lid`, OP, async () => { throw new Error("no mapping"); }), false);
  assert.equal(await isOperatorId(`${OP}@s.whatsapp.net`, null, pn), false, "no operator configured");
  const isOp = (j: string) => isOperatorId(j, OP, pn);
  assert.deepEqual(await unexpectedMembers([`${OP_LID}@lid`, `${BOT}@s.whatsapp.net`], [`${BOT}:3@${WA}`], isOp), []);
  assert.deepEqual(await unexpectedMembers([`${OP}@s.whatsapp.net`, `${STRANGER}@s.whatsapp.net`], [BOT], isOp), [`${STRANGER}@s.whatsapp.net`]);
});

test("a request is claimed once, and a creation whose outcome is unknown is never repeated", async () => {
  const db = tmpDb();
  const store = new IntakeStore(db);
  request(db, "1", "create", "#1 a");
  const [req] = store.pendingRequests();
  const nonce = newNonce();
  assert.match(nonce, /^[0-9a-f]{16}$/);
  assert.equal(store.claimCreate(req.id, nonce), true);
  assert.equal(store.claimCreate(req.id, newNonce()), false, "a second pass does not claim it");
  const row = new DatabaseSync(db).prepare(`SELECT status, nonce FROM intake_group_requests WHERE id = ?`).get(req.id) as any;
  assert.deepEqual([row.status, row.nonce], ["creating", nonce], "the claim and the nonce are one write");
  store.markCalling(req.id);
  // The bot stopped mid-creation: after the stuck-claim limit the request is
  // recorded as unknown with its nonce; create is not called again.
  const { api, calls } = fakeApi();
  let now = Date.now() + 11 * 60 * 1000;
  const { d } = deps(store, api, { nowMs: () => now });
  const ex = new GroupExecutor(d);
  await ex.tick();
  assert.equal(calls.length, 0);
  assert.equal(store.group("1")?.status, "unknown");
  assert.equal(store.group("1")?.token, nonce);
  assert.equal(store.createdTimes().length, 1, "an unknown outcome counts against the daily limit");
  // A search that does not find it changes nothing.
  now += 3 * 60 * 1000;
  await ex.tick();
  assert.equal(store.group("1")?.status, "unknown", "not found is not evidence of absence");
  assert.equal(calls.length, 0);
  // A claim stuck before the create call was marked never called create.
  request(db, "2", "create", "#2 b");
  const r2 = store.pendingRequests().find((r) => r.itemKey === "2")!;
  store.claimCreate(r2.id, newNonce(), now);
  now += 11 * 60 * 1000;
  await ex.tick();
  assert.equal(store.group("2")?.status, "fallback");
});

/** An API whose create happens at WhatsApp and then times out, with other
 *  groups the bot participates in. */
function timingOut(extra: Array<{ jid: string; subject: string; members: string[] }>) {
  const base = fakeApi();
  base.groups.push(...extra);
  const api: GroupApi = {
    ...base.api,
    create: async (subject, participants) => {
      await base.api.create(subject, participants);
      throw new Error("timed out after 30000 ms");
    },
  };
  return { api, base };
}

test("a group found by its nonce is quarantined and left, never activated", async () => {
  const db = tmpDb();
  const store = new IntakeStore(db);
  // Two groups with a nonce-like suffix that is not ours.
  const decoys = [
    { jid: gjid("71"), subject: `#7 g ${nonceSuffix(newNonce()).trim()}`, members: [`${OP}@s.whatsapp.net`, `${BOT}@s.whatsapp.net`] },
    { jid: gjid("72"), subject: `#7 g ${nonceSuffix(newNonce()).trim()}`, members: [`${OP}@s.whatsapp.net`, `${BOT}@s.whatsapp.net`, `${STRANGER}@s.whatsapp.net`] },
  ];
  const { api, base } = timingOut(decoys);
  let now = Date.parse("2026-09-24T12:00:00Z");
  const active: string[] = [];
  const ex = new GroupExecutor(deps(store, api, { nowMs: () => now, onActive: (j) => active.push(j) }).d);
  request(db, "7", "create", "#7 g", new Date(now).toISOString());
  await ex.tick();
  assert.equal(store.group("7")?.status, "unknown", "a timeout is not a rejection");
  now += 3 * 60 * 1000;
  await ex.tick();
  assert.equal(store.group("7")?.status, "closed", "found by its nonce, quarantined, then left");
  assert.match(store.group("7")?.reason ?? "", /members exact/);
  assert.deepEqual(active, [], "never added to the target allowlist");
  assert.deepEqual(base.groups.map((g) => g.jid), decoys.map((g) => g.jid), "the decoys were not touched");
});

test("a found group with extra members is left; several matches stay unknown", async () => {
  // The right nonce, with a stranger in the group.
  const db = tmpDb();
  const store = new IntakeStore(db);
  const base = fakeApi();
  const api: GroupApi = {
    ...base.api,
    create: async (subject) => {
      base.groups.push({ jid: gjid("81"), subject, members: [`${OP}@s.whatsapp.net`, `${BOT}@s.whatsapp.net`, `${STRANGER}@s.whatsapp.net`] });
      throw new Error("timed out");
    },
  };
  let now = Date.parse("2026-09-24T12:00:00Z");
  const active: string[] = [];
  const ex = new GroupExecutor(deps(store, api, { nowMs: () => now, onActive: (j) => active.push(j) }).d);
  request(db, "8", "create", "#8 h", new Date(now).toISOString());
  await ex.tick();
  now += 3 * 60 * 1000;
  await ex.tick();
  assert.equal(store.group("8")?.status, "closed");
  assert.match(store.group("8")?.reason ?? "", /unexpected members/);
  assert.deepEqual(active, []);
  assert.equal(base.groups.length, 0, "the bot left it");

  // Two groups carry our nonce: nothing is decided.
  const db2 = tmpDb();
  const store2 = new IntakeStore(db2);
  const base2 = fakeApi();
  const api2: GroupApi = {
    ...base2.api,
    create: async (subject) => {
      for (const n of ["91", "92"]) base2.groups.push({ jid: `1203630000000000${n}@g.us`, subject, members: [`${OP}@s.whatsapp.net`, `${BOT}@s.whatsapp.net`] });
      throw new Error("timed out");
    },
  };
  let now2 = Date.parse("2026-09-24T12:00:00Z");
  const ex2 = new GroupExecutor(deps(store2, api2, { nowMs: () => now2 }).d);
  request(db2, "9", "create", "#9 i", new Date(now2).toISOString());
  await ex2.tick();
  now2 += 3 * 60 * 1000;
  await ex2.tick();
  assert.equal(store2.group("9")?.status, "unknown");
  assert.equal(base2.groups.length, 2);
});

test("an unresolved creation stays unknown, is reported at most once a day, and the operator resolves it", async () => {
  const db = tmpDb();
  const store = new IntakeStore(db);
  const base = fakeApi();
  const api: GroupApi = {
    ...base.api,
    create: async () => {
      throw Object.assign(new Error("connection closed"), { output: { statusCode: 428 } });
    },
    listParticipating: async () => {
      throw new Error("timed out");
    },
  };
  let now = Date.parse("2026-09-24T12:00:00Z");
  const { d, alerts, alertKeys } = deps(store, api, { nowMs: () => now });
  const ex = new GroupExecutor(d);
  request(db, "8", "create", "#8 h", new Date(now).toISOString());
  await ex.tick();
  for (let i = 0; i < 30 * 24; i++) {
    now += 3 * 60 * 1000; // 36 hours
    await ex.tick();
  }
  assert.equal(store.group("8")?.status, "unknown");
  assert.ok(alerts.some((a) => /still not known/.test(a) && /group-resolve 8 --left/.test(a)), "reported after the limit");
  assert.equal(new Set(alertKeys).size, 2, "one alert key per day (the outbound queue sends each key once)");
  // The operator resolved it by hand.
  new DatabaseSync(db)
    .prepare(`INSERT INTO intake_group_requests (item_key, action, subject, enqueued_at, status) VALUES ('8', 'resolve', 'absent', ?, 'pending')`)
    .run(new Date(now).toISOString());
  await ex.tick();
  assert.equal(store.group("8")?.status, "closed");
  assert.match(store.group("8")?.reason ?? "", /resolved by the operator/);
  assert.equal(classifyCreateError(new Error("no live connection")), "rejected");
  assert.equal(classifyCreateError(Object.assign(new Error("x"), { output: { statusCode: 403 } })), "rejected");
  assert.equal(classifyCreateError(Object.assign(new Error("x"), { output: { statusCode: 408 } })), "unknown");
  assert.equal(classifyCreateError(new Error("anything else")), "unknown");
});

test("an item closed before or during group creation ends without a group", async () => {
  const db = tmpDb();
  const store = new IntakeStore(db);
  const { api, calls } = fakeApi();
  // Closed before the bot handled the create request: no group.
  request(db, "2", "create", "#2 b");
  request(db, "2", "close", null);
  const ex = new GroupExecutor(deps(store, api).d);
  await ex.tick();
  assert.equal(calls.length, 0);
  assert.equal(store.group("2")?.status, "fallback");
  // Closed while the create call ran: the new group is left at once.
  const racing: GroupApi = {
    create: async (subject, participants) => {
      request(db, "3", "close", null);
      return api.create(subject, participants);
    },
    leave: api.leave,
    listParticipating: api.listParticipating,
  };
  request(db, "3", "create", "#3 c");
  const closed: string[] = [];
  await new GroupExecutor(deps(store, racing, { onClosed: (j) => closed.push(j) }).d).tick();
  assert.equal(store.group("3")?.status, "closed");
  assert.equal(calls.filter((c) => c.startsWith("leave")).length, 1);
  assert.equal(closed.length, 1);
});

test("a new group with an unexpected member starts disabled and alerts the operator", async () => {
  const db = tmpDb();
  const store = new IntakeStore(db);
  const api: GroupApi = {
    create: async () => ({ jid: GROUP, members: [`${OP}@s.whatsapp.net`, `${BOT}@s.whatsapp.net`, `${STRANGER}@s.whatsapp.net`] }),
    leave: async () => {},
    listParticipating: async () => [],
  };
  const { d, alerts, seeded } = deps(store, api);
  request(db, "4", "create", "#4 d");
  await new GroupExecutor(d).tick();
  assert.equal(seeded.length, 1);
  assert.match(seeded[0].reason ?? "", /unexpected members/);
  assert.match(alerts[0], /1 member\(s\) besides the bot and you/);
});

test("leaving is retried with backoff and closed_at is set only when the bot left", async () => {
  const db = tmpDb();
  const store = new IntakeStore(db);
  let fail = true;
  let member: boolean | null = null;
  const api: GroupApi = {
    create: async () => ({ jid: GROUP, members: [`${OP}@s.whatsapp.net`, `${BOT}@s.whatsapp.net`] }),
    leave: async () => {
      if (fail) throw new Error("timed out");
    },
    isMember: async () => member,
    listParticipating: async () => [],
  };
  let now = Date.parse("2026-09-24T12:00:00Z");
  const { d, alerts } = deps(store, api, { nowMs: () => now });
  const ex = new GroupExecutor(d);
  request(db, "5", "create", "#5 e", new Date(now).toISOString());
  await ex.tick();
  request(db, "5", "close", null, new Date(now).toISOString());
  await ex.tick();
  const row = () => new DatabaseSync(db).prepare(`SELECT status, attempts, next_attempt_at FROM intake_group_requests WHERE action = 'close'`).get() as any;
  assert.equal(store.group("5")?.status, "active", "not closed while leaving fails");
  assert.equal(row().attempts, 1);
  assert.equal(row().next_attempt_at, new Date(now + closeBackoffMs(1)).toISOString());
  await ex.tick();
  assert.equal(row().attempts, 1, "not retried before the backoff");
  now += closeBackoffMs(1);
  await ex.tick();
  assert.equal(row().attempts, 2);
  // The bot is confirmed out of the group: closed.
  member = false;
  now += closeBackoffMs(2);
  await ex.tick();
  assert.equal(store.group("5")?.status, "closed");
  const closedAt = (new DatabaseSync(db).prepare(`SELECT closed_at FROM intake_groups WHERE item_key = '5'`).get() as any).closed_at;
  assert.ok(closedAt);
  // A close that keeps failing is given up after MAX_CLOSE_ATTEMPTS.
  const db2 = tmpDb();
  const store2 = new IntakeStore(db2);
  let now2 = Date.parse("2026-09-24T12:00:00Z");
  const x = deps(store2, { ...api, isMember: async () => null }, { nowMs: () => now2 });
  const ex2 = new GroupExecutor(x.d);
  request(db2, "6", "create", "#6 f", new Date(now2).toISOString());
  await ex2.tick();
  request(db2, "6", "close", null, new Date(now2).toISOString());
  for (let i = 1; i <= MAX_CLOSE_ATTEMPTS; i++) {
    await ex2.tick();
    now2 += closeBackoffMs(i);
  }
  const r2 = new DatabaseSync(db2).prepare(`SELECT status FROM intake_group_requests WHERE action = 'close'`).get() as any;
  assert.equal(r2.status, "failed");
  assert.equal(store2.group("6")?.status, "active");
  assert.match(x.alerts[0], /Leave the group by hand/);
  assert.equal(alerts.length, 0);
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
