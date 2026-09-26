// Issue-pipeline surface on WhatsApp (ADR-036): routing operator messages to
// items, the DM session's decision block, and the one-time cleanup of the
// removed per-item groups.

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { ChatSessionStore, OutboundQueueStore } from "./db.js";
import { parseToml } from "./config.js";
import {
  ANSWER_WINDOW_MS,
  CHAT_KEY,
  cleanupLegacyGroups,
  DM_KEY,
  IntakeStore,
  LEGACY_LEAVE_ALERT_AFTER,
  admitDm,
  dmChatIntake,
  isOperatorId,
  OperatorLidCache,
  OPERATOR_LID_TTL_MS,
  stampBatch,
  waSeconds,
  type OperatorIds,
  routeDm,
  routeOperatorDm,
  type LegacyGroupApi,
} from "./intake.js";
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

const quiet = { info: () => {}, warn: () => {} };
const BOT = ["55119", "88888888"].join("");
const WA = ["s", "whatsapp", "net"].join(".");
const OP_LID = ["1234567", "89012345"].join("");
const STRANGER = ["55119", "77777777"].join("");
/** The operator's identities with only the phone digits allowlisted. */
/** The operator's identities: his phone, and `lids` as WHATSAPP_OPERATOR_LIDS. */
const ops = (op: string | null = OP, ...lids: string[]): OperatorIds => ({
  operatorId: op,
  operatorLids: new Set(lids),
});
/** Another contact's LID, allowlisted for DMs but not the operator. */
const OTHER_LID = ["55555", "5555555555"].join("");

test("DM messages go to an item by its marker or by a quoted pipeline message", () => {
  const has = (n: string) => n === "3";
  assert.deepEqual(routeDm("#3 approve", null, has), { item: "3", text: "approve" });
  assert.deepEqual(routeDm("  #3   use option B", null, has), { item: "3", text: "use option B" });
  assert.equal(routeDm("#4 approve", null, has), null, "no DM thread for #4");
  assert.equal(routeDm("#1 priority today is the report", null, has), null, "an ordinary message keeps its #");
  assert.equal(routeDm("hello", null, has), null);
  assert.deepEqual(routeDm("approve", "7", has), { item: "7", text: "approve" });
  assert.deepEqual(routeDm("#7 looks good", "7", has), { item: "7", text: "looks good" });
  // A reply to a question or note the pipeline sent in the DM, and the next
  // message while the pipeline waits for an answer, reach it without an item.
  assert.deepEqual(routeDm("yes", DM_KEY, has), { item: DM_KEY, text: "yes" });
  assert.deepEqual(routeDm("  yes ", null, has, true), { item: DM_KEY, text: "yes" });
  assert.deepEqual(routeDm("#3 no", null, has, true), { item: "3", text: "no" }, "a marker still names the item");
  assert.equal(routeDm("yes", null, has, false), null, "no question waits: the chat session gets it");
});

test("only the operator's own DM reaches the pipeline, with how the message was written", async () => {
  const has = (n: string) => n === "3";
  const noLid = async () => null;
  const base = { operator: ops(), pnForLid: noLid, quotedItem: null, hasDmThread: has };
  const opChat = `${OP}@s.whatsapp.net`;
  assert.deepEqual(await routeOperatorDm({ ...base, chatId: opChat, text: "#3 release it", inputKind: "text" }), {
    item: "3",
    text: "release it",
    inputKind: "text",
  });
  // A transcribed voice note or a forwarded message keeps its kind; the
  // pipeline confirms a decision from it before it runs.
  assert.equal((await routeOperatorDm({ ...base, chatId: opChat, text: "#3 release it", inputKind: "voice" }))?.inputKind, "voice");
  assert.equal((await routeOperatorDm({ ...base, chatId: opChat, text: "#3 release it", inputKind: "forwarded" }))?.inputKind, "forwarded");
  // Another sender's message never reaches the pipeline, not even while it
  // waits for the operator's answer.
  const other = `${["55119", "88888888"].join("")}@s.whatsapp.net`;
  assert.equal(await routeOperatorDm({ ...base, chatId: other, text: "#3 release it", inputKind: "text" }), null);
  assert.equal(await routeOperatorDm({ ...base, chatId: other, text: "yes", expectingAnswer: true, inputKind: "text" }), null);
  assert.deepEqual(await routeOperatorDm({ ...base, chatId: opChat, text: "yes", expectingAnswer: true, inputKind: "voice" }), {
    item: DM_KEY,
    text: "yes",
    inputKind: "voice",
  });
  // The operator in LID form, resolved through the connection's mapping.
  const lid = ["123456789012345", "lid"].join("@");
  const viaLid = await routeOperatorDm({ ...base, chatId: lid, pnForLid: async () => opChat, text: "#3 release it", inputKind: "text" });
  assert.equal(viaLid?.item, "3");
  // An unknown LID is not the operator.
  assert.equal(await routeOperatorDm({ ...base, chatId: lid, text: "#3 release it", inputKind: "text" }), null);
});

test("the next DM message answers a question the pipeline asked, for 15 minutes", () => {
  const db = tmpDb();
  const store = new IntakeStore(db);
  const out = new OutboundQueueStore(db);
  const t0 = Date.now();
  assert.equal(store.expectsDmAnswer(t0), false, "nothing asked");
  // A note that asks nothing does not open the window.
  const note = out.enqueue({ target: "dm", body: "Nothing was done for item #1.", source: "intake:note", dedupKey: "n1" });
  out.markSent(note, "WANOTE");
  assert.equal(store.expectsDmAnswer(t0), false);
  // A question not sent yet cannot be answered.
  const ask = out.enqueue({ target: "dm", body: "Release item #1? Answer yes or no.", source: "intake:ask", dedupKey: "a1" });
  assert.equal(store.expectsDmAnswer(t0), false);
  out.markSent(ask, "WAASK");
  const sentAt = Date.parse(
    (new DatabaseSync(db).prepare(`SELECT sent_at FROM outbound_queue WHERE id = ?`).get(ask) as { sent_at: string }).sent_at,
  );
  assert.equal(store.expectsDmAnswer(sentAt + 1000), true);
  assert.equal(store.expectsDmAnswer(sentAt + ANSWER_WINDOW_MS + 1), false, "the window closes after 15 minutes");
  // Replies to either message reach the pipeline without an item.
  assert.equal(store.itemForSentMessage("WAASK"), DM_KEY);
  assert.equal(store.itemForSentMessage("WANOTE"), DM_KEY);
  // The first DM message after the question is the answer; later ones go
  // to the chat session again. A group message does not count.
  store.recordInbound({ itemKey: "4", chatId: GROUP, waMsgId: "g1", text: "hi", inputKind: "text", sender: "operator", nowMs: sentAt + 2000 });
  assert.equal(store.expectsDmAnswer(sentAt + 3000), true);
  store.recordInbound({ itemKey: DM_KEY, chatId: `${OP}@s.whatsapp.net`, waMsgId: "d1", text: "yes", inputKind: "text", sender: "operator", nowMs: sentAt + 4000 });
  assert.equal(store.expectsDmAnswer(sentAt + 5000), false);
});

test("a batch's messages keep the arrival stamped before handling, and WhatsApp's own timestamp", async () => {
  const db = tmpDb();
  const store = new IntakeStore(db);
  const out = new OutboundQueueStore(db);
  const opChat = `${OP}@s.whatsapp.net`;
  const t0 = Date.now() - 60_000;
  // "cancel item" and "yes" arrive in one upsert batch.
  const batch = [
    { id: "b1", text: "cancel item", ts: 1790000000 },
    { id: "b2", text: "yes", ts: { low: 1790000000, toNumber: () => 1790000000 } },
  ];
  const stamps = stampBatch(batch, (m) => m.ts, t0);
  // Handling the first message takes time; its question is sent before the
  // second message is even recorded.
  store.recordInbound({ itemKey: "chat", chatId: opChat, waMsgId: "b1", text: "cancel item", inputKind: "text", sender: "operator", arrival: stamps.get(batch[0]) });
  const q = out.enqueue({ target: "dm", body: "Cancel item #1? Answer yes or no.", source: "intake:ask", dedupKey: "intake:answer:b1" });
  out.markSent(q, "WAQ", 1790000003);
  // A voice "yes" is recorded after its transcription finished, later still.
  store.recordInbound({ itemKey: "chat", chatId: opChat, waMsgId: "b2", text: "yes", inputKind: "voice", sender: "operator", arrival: stamps.get(batch[1]) });
  const d = new DatabaseSync(db);
  const yes = d.prepare(`SELECT received_at, wa_ts FROM intake_inbound WHERE wa_msg_id = 'b2'`).get() as { received_at: string; wa_ts: number };
  const sent = d.prepare(`SELECT sent_at, wa_ts FROM outbound_queue WHERE id = ?`).get(q) as { sent_at: string; wa_ts: number };
  assert.equal(yes.received_at, new Date(t0).toISOString(), "the arrival, not the recording time");
  assert.ok(yes.received_at < sent.sent_at, "the yes arrived before the question was sent");
  assert.equal(yes.wa_ts, 1790000000);
  assert.equal(sent.wa_ts, 1790000003, "the question's WhatsApp timestamp from the send result");
  assert.equal(waSeconds("1790000001"), 1790000001);
  assert.equal(waSeconds(undefined), null);
});

test("an appended line is in the claimed body or refused, never lost", () => {
  const db = tmpDb();
  const out = new OutboundQueueStore(db);
  const d = new DatabaseSync(db);
  // The Rust append (whatsapp_queue::append_to_pending): only a pending row.
  const append = (key: string) =>
    Number(d.prepare(`UPDATE outbound_queue SET body = body || ? WHERE dedup_key = ? AND status = 'pending'`).run("\n\nAlso received: 'x'", key).changes) === 1;
  // Append first, then the claim: the claimed body has the line.
  const a = out.enqueue({ target: "dm", body: "Question A?", source: "intake:ask", dedupKey: "qa" });
  assert.equal(append("qa"), true);
  const claimedA = out.claimForSend(a, "WA-A");
  assert.ok(claimedA?.body.endsWith("Also received: 'x'"), claimedA?.body);
  // Claim first, then the append: refused, so the caller queues the
  // separate note; the claimed body is what is sent.
  const b = out.enqueue({ target: "dm", body: "Question B?", source: "intake:ask", dedupKey: "qb" });
  const claimedB = out.claimForSend(b, "WA-B");
  assert.equal(claimedB?.body, "Question B?");
  assert.equal(append("qb"), false, "refused on a claimed row");
  // A claimed row is not claimed twice while its send is fresh.
  assert.equal(out.claimForSend(b, "WA-B2"), null);
});

test("a remapped LID stops being the operator", async () => {
  let map: Record<string, string> = { [`${OP_LID}@lid`]: `${OP}@s.whatsapp.net` };
  const pn = async (lid: string) => map[lid] ?? null;
  const cache = new OperatorLidCache();
  const t0 = 1_000_000;
  assert.equal(await isOperatorId(`${OP_LID}@lid`, ops(), pn), true);
  cache.note(OP_LID, t0);
  assert.deepEqual([...cache.current(t0 + 1000)], [OP_LID]);
  // The mapping now resolves the LID to someone else: the live check fails
  // at once, and the next refresh drops it from the synchronous checks.
  map = { [`${OP_LID}@lid`]: `${STRANGER}@s.whatsapp.net` };
  assert.equal(await isOperatorId(`${OP_LID}@lid`, ops(), pn), false);
  await cache.refresh(OP, pn, t0 + 2000);
  assert.deepEqual([...cache.current(t0 + 2000)], []);
  // An entry counts only within the TTL unless a refresh verifies it again.
  map = { [`${OP_LID}@lid`]: `${OP}@s.whatsapp.net` };
  cache.note(OP_LID, t0);
  assert.deepEqual([...cache.current(t0 + OPERATOR_LID_TTL_MS + 1)], []);
  await cache.refresh(OP, pn, t0 + OPERATOR_LID_TTL_MS + 1);
  assert.deepEqual([...cache.current(t0 + OPERATOR_LID_TTL_MS + 2)], [OP_LID]);
  // Every verification and drop is mirrored for the Rust side.
  const db = tmpDb();
  const store = new IntakeStore(db);
  const mirrored = new OperatorLidCache(OPERATOR_LID_TTL_MS, {
    verified: (d, at) => store.markOperatorLidVerified(d, at),
    dropped: (d) => store.forgetOperatorLid(d),
  });
  mirrored.note(OP_LID, t0);
  const rows = () => new DatabaseSync(db).prepare(`SELECT digits, verified_at FROM operator_lid_verified`).all() as Array<{ digits: string; verified_at: string }>;
  assert.deepEqual(rows().map((r) => r.digits), [OP_LID]);
  assert.equal(rows()[0].verified_at, new Date(t0).toISOString());
  await mirrored.refresh(OP, async () => `${STRANGER}@s.whatsapp.net`, t0 + 1);
  assert.deepEqual(rows(), [], "a remapped LID is removed for the Rust side too");
  // A failing mapping drops it too; WHATSAPP_OPERATOR_LIDS does not depend on it.
  await cache.refresh(OP, async () => { throw new Error("mapping gone"); }, t0 + OPERATOR_LID_TTL_MS + 3);
  assert.deepEqual([...cache.current(t0 + OPERATOR_LID_TTL_MS + 3)], []);
  assert.equal(await isOperatorId(`${OP_LID}@lid`, ops(OP, OP_LID), async () => { throw new Error("mapping gone"); }), true);
});

test("the DM gate admits other allowlisted contacts as normal chat and the operator by the shared check", async () => {
  const allowedDm = new Set([OP, OTHER_LID]);
  const noMap = async () => null;
  const mapped = async (lid: string) => (lid === `${OP_LID}@lid` ? `${OP}@s.whatsapp.net` : null);
  // A second allowlisted LID: admitted, never the operator.
  assert.deepEqual(await admitDm({ chatId: `${OTHER_LID}@lid`, allowedDm, operator: ops(), pnForLid: noMap }), { admitted: true, operator: false });
  // The operator's LID that only the mapping resolves: admitted and the operator.
  assert.deepEqual(await admitDm({ chatId: `${OP_LID}@lid`, allowedDm, operator: ops(), pnForLid: mapped }), { admitted: true, operator: true });
  // The operator's LID from WHATSAPP_OPERATOR_LIDS, no mapping.
  assert.deepEqual(await admitDm({ chatId: `${OP_LID}@lid`, allowedDm, operator: ops(OP, OP_LID), pnForLid: noMap }), { admitted: true, operator: true });
  // An unknown LID: not admitted.
  assert.deepEqual(await admitDm({ chatId: `${OP_LID}@lid`, allowedDm, operator: ops(), pnForLid: noMap }), { admitted: false, operator: false });
  // The operator's phone.
  assert.deepEqual(await admitDm({ chatId: `${OP}@${WA}`, allowedDm, operator: ops(), pnForLid: noMap }), { admitted: true, operator: true });
  // The other contact's chat gets no decision block and no stored text.
  assert.deepEqual(
    await dmChatIntake({ chatId: `${OTHER_LID}@lid`, operator: ops(), pnForLid: noMap, chatBlock: () => "BLOCK" }),
    { record: false, block: "" },
  );
});

test("the operator's DM in LID form gets the decision block and its text is kept; an unknown LID gets neither", async () => {
  const block = "[Issue pipeline: decisions waiting for the operator.]\n- item #1: plan v1 is waiting for your approval.";
  const chatBlock = () => block;
  const noMap = async () => null;
  const lidChat = `${OP_LID}@lid`;
  // A DM chat keyed `<digits>@lid`, those digits in the allowlist.
  assert.deepEqual(await dmChatIntake({ chatId: lidChat, operator: ops(OP, OP_LID), pnForLid: noMap, chatBlock }), { record: true, block });
  // The same LID resolved to the operator's phone through the mapping.
  const mapped = async (lid: string) => (lid === lidChat ? `${OP}@s.whatsapp.net` : null);
  assert.deepEqual(await dmChatIntake({ chatId: lidChat, operator: ops(), pnForLid: mapped, chatBlock }), { record: true, block });
  // The phone form.
  assert.deepEqual(await dmChatIntake({ chatId: `${OP}@${WA}`, operator: ops(), pnForLid: noMap, chatBlock }), { record: true, block });
  // An unknown LID (not allowlisted, no mapping), and another sender.
  const unknown = `${["98765", "4321098765"].join("")}@lid`;
  assert.deepEqual(await dmChatIntake({ chatId: unknown, operator: ops(OP, OP_LID), pnForLid: noMap, chatBlock }), { record: false, block: "" });
  assert.deepEqual(await dmChatIntake({ chatId: `${STRANGER}@${WA}`, operator: ops(), pnForLid: noMap, chatBlock }), { record: false, block: "" });
  // The fast paths use the same rule.
  const route = (chatId: string, operator: OperatorIds, pnForLid: typeof noMap | typeof mapped) =>
    routeOperatorDm({ chatId, operator, pnForLid, text: "yes", quotedItem: null, hasDmThread: () => false, expectingAnswer: true, inputKind: "text" });
  assert.equal((await route(lidChat, ops(OP, OP_LID), noMap))?.item, DM_KEY);
  assert.equal((await route(lidChat, ops(), mapped))?.item, DM_KEY);
  assert.equal(await route(unknown, ops(OP, OP_LID), noMap), null);
});

test("the DM session's decision block is read from the table the pipeline writes", () => {
  const db = tmpDb();
  const store = new IntakeStore(db);
  assert.equal(store.chatBlock(), "", "nothing written yet");
  new DatabaseSync(db)
    .prepare(`INSERT INTO intake_chat_block (id, block, updated_at) VALUES (1, ?, 't')`)
    .run("[Issue pipeline: decisions waiting for the operator.]\n- item #2: held for hidden content (1 findings).");
  assert.match(store.chatBlock(), /item #2: held/);
  // An operator DM message for the chat session is stored under the chat
  // key, for `interpret-latest`, and marked as the operator's.
  assert.equal(
    store.recordInbound({ itemKey: CHAT_KEY, chatId: `${OP}@s.whatsapp.net`, waMsgId: "c1", text: "approve it", inputKind: "text", sender: "operator" }),
    true,
  );
  const row = new DatabaseSync(db).prepare(`SELECT item_key, sender FROM intake_inbound WHERE wa_msg_id = 'c1'`).get() as {
    item_key: string;
    sender: string;
  };
  assert.deepEqual({ ...row }, { item_key: "chat", sender: "operator" });
});

test("operator messages are stored once and quoted messages map to their item", () => {
  const db = tmpDb();
  const store = new IntakeStore(db);
  assert.equal(store.recordInbound({ itemKey: "2", chatId: GROUP, waMsgId: "m1", text: "hi", inputKind: "voice", sender: "operator" }), true);
  assert.equal(store.recordInbound({ itemKey: "2", chatId: GROUP, waMsgId: "m1", text: "hi", inputKind: "text", sender: "operator" }), false);
  const row = new DatabaseSync(db).prepare(`SELECT input_kind, sender FROM intake_inbound WHERE wa_msg_id = 'm1'`).get() as {
    input_kind: string;
    sender: string;
  };
  assert.equal(row.input_kind, "voice", "how the message was written is stored");
  assert.equal(row.sender, "operator", "the identity check is recorded for the pipeline");
  const out = new OutboundQueueStore(db);
  const id = out.enqueue({ target: "dm", body: "[#2] plan", source: "intake:2", dedupKey: "intake:2:m1" });
  out.markInFlight(id, "WAMSG1");
  assert.equal(store.itemForSentMessage("WAMSG1"), "2");
  assert.equal(store.itemForSentMessage("OTHER"), null);
  assert.equal(store.hasDmThread("2"), true);
  assert.equal(store.hasDmThread("3"), false);
});

test("only the operator's own identity counts, in phone or LID form", async () => {
  const pn = async (lid: string) => (lid === `${OP_LID}@lid` ? `${OP}@s.whatsapp.net` : null);
  assert.equal(await isOperatorId(`${OP}@s.whatsapp.net`, ops(), pn), true);
  assert.equal(await isOperatorId(`${OP}:12@${WA}`, ops(), pn), true, "any device");
  assert.equal(await isOperatorId(`${OP_LID}@lid`, ops(), pn), true, "LID mapped to the operator's number");
  assert.equal(await isOperatorId(`${STRANGER}@s.whatsapp.net`, ops(), pn), false);
  assert.equal(await isOperatorId(`${OP_LID}@lid`, ops(), async () => { throw new Error("no mapping"); }), false);
  assert.equal(await isOperatorId(`${OP}@s.whatsapp.net`, ops(null), pn), false, "no operator configured");
  // An operator LID from WHATSAPP_OPERATOR_LIDS counts without a mapping.
  const failing = async () => { throw new Error("no mapping"); };
  assert.equal(await isOperatorId(`${OP_LID}@lid`, ops(OP, OP_LID), failing), true, "operator LID");
  // Only in LID form: those digits in a phone JID are not the operator.
  assert.equal(await isOperatorId(`${OP_LID}@${WA}`, ops(OP, OP_LID), failing), false);
  // Another contact's LID is never the operator, allowlisted or not.
  assert.equal(await isOperatorId(`${OTHER_LID}@lid`, ops(OP, OP_LID), failing), false);
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

[intake.texts]
unclear = "?"
`);
  assert.equal(t.intake.enabled, true);
  assert.deepEqual(
    t.intake.repos.map((r: any) => r.repo),
    ["owner/one", "owner/two"],
  );
  assert.equal(t.intake.repos[0].test_command, "npm test");
  assert.equal(t.intake.texts.unclear, "?");
});

// ── the one-time cleanup of the removed per-item groups ─────────────────

/** A whatsapp.db as an earlier version left it: the group tables with rows. */
function legacyDb(rows: Array<{ item: string; jid: string | null; status: string; token?: string }>): string {
  const db = tmpDb();
  new IntakeStore(db);
  const raw = new DatabaseSync(db);
  raw.exec(`
    CREATE TABLE intake_group_requests (id INTEGER PRIMARY KEY AUTOINCREMENT, item_key TEXT NOT NULL, action TEXT NOT NULL,
      subject TEXT, enqueued_at TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'pending', result TEXT, handled_at TEXT,
      nonce TEXT, calling_at TEXT);
    CREATE TABLE intake_groups (item_key TEXT PRIMARY KEY, jid TEXT, subject TEXT, status TEXT NOT NULL, reason TEXT,
      created_at TEXT NOT NULL, closed_at TEXT, members_json TEXT, token TEXT, checked_at TEXT);
  `);
  raw.prepare(`INSERT INTO intake_group_requests (item_key, action, enqueued_at) VALUES ('1', 'create', 't')`).run();
  for (const r of rows) {
    raw
      .prepare(`INSERT INTO intake_groups (item_key, jid, status, created_at, token) VALUES (?, ?, ?, 't', ?)`)
      .run(r.item, r.jid, r.status, r.token ?? null);
  }
  return db;
}

function tables(db: string): string[] {
  return (new DatabaseSync(db).prepare(`SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'intake_group%' ORDER BY name`).all() as Array<{
    name: string;
  }>).map((r) => r.name);
}

function legacyApi(opts: { failLeave?: Set<string>; member?: boolean | null; groups?: Array<{ jid: string; subject: string }> } = {}) {
  const calls: string[] = [];
  const api: LegacyGroupApi = {
    leave: async (jid) => {
      calls.push(`leave ${jid}`);
      if (opts.failLeave?.has(jid)) throw new Error("rate-overlimit");
    },
    isMember: async () => opts.member ?? null,
    listParticipating: async () => {
      calls.push("list");
      return opts.groups ?? [];
    },
  };
  return { api, calls };
}

test("at start the bot leaves every open old group, marks it closed, then drops the group tables", async () => {
  const g1 = ["120363000000000011", "g.us"].join("@");
  const g2 = ["120363000000000012", "g.us"].join("@");
  const g3 = ["120363000000000013", "g.us"].join("@");
  const db = legacyDb([
    { item: "1", jid: g1, status: "active" },
    { item: "2", jid: g2, status: "quarantined" },
    { item: "3", jid: null, status: "unknown", token: "abc123" },
    { item: "4", jid: null, status: "fallback" },
    { item: "5", jid: null, status: "closed" },
    { item: "6", jid: null, status: "unknown", token: "def456" },
  ]);
  const store = new IntakeStore(db);
  const { api, calls } = legacyApi({ groups: [{ jid: g3, subject: "#3 Fix ~abc123" }] });
  const alerts: string[] = [];
  const r = await cleanupLegacyGroups({ store, api, alertOperator: (t) => alerts.push(t), log: quiet });
  assert.deepEqual(r.left, ["1", "2", "3"], "active, quarantined, and an unknown one found by its nonce");
  assert.deepEqual(r.failed, []);
  assert.deepEqual(calls, [`leave ${g1}`, `leave ${g2}`, "list", `leave ${g3}`], "the groups are listed once");
  assert.equal(r.dropped, true);
  assert.deepEqual(tables(db), [], "both group tables are dropped");
  assert.equal(alerts.length, 0);
  // A later start finds nothing to do.
  const again = await cleanupLegacyGroups({ store: new IntakeStore(db), api, alertOperator: (t) => alerts.push(t), log: quiet });
  assert.deepEqual(again, { left: [], failed: [], dropped: true, kept: [] });
});

test("a failed leave keeps its row for the next start and is reported only after 3 failures", async () => {
  const g1 = ["120363000000000021", "g.us"].join("@");
  const g2 = ["120363000000000022", "g.us"].join("@");
  const db = legacyDb([
    { item: "1", jid: g1, status: "active" },
    { item: "2", jid: g2, status: "active" },
  ]);
  const alerts: Array<{ text: string; key: string }> = [];
  const run = (failLeave: Set<string>, member: boolean | null = null) =>
    cleanupLegacyGroups({
      store: new IntakeStore(db),
      api: legacyApi({ failLeave, member }).api,
      alertOperator: (text, key) => alerts.push({ text, key }),
      log: quiet,
    });
  let r = await run(new Set([g1]));
  assert.deepEqual([r.left, r.failed, r.dropped], [["2"], ["1"], false]);
  assert.deepEqual(tables(db), ["intake_groups"], "the group table stays while a group is open; the settled requests go");
  for (let start = 2; start < LEGACY_LEAVE_ALERT_AFTER; start++) {
    r = await run(new Set([g1]));
    assert.deepEqual(r.failed, ["1"]);
  }
  assert.equal(alerts.length, 0, "not reported before the third failure");
  r = await run(new Set([g1]));
  assert.equal(alerts.length, 1);
  assert.match(alerts[0].text, /Item #1: leaving its old WhatsApp group failed 3 times \(rate-overlimit\)/);
  assert.equal(alerts[0].key, "intake:group-cleanup:1", "one dedup key: the outbound queue sends it once");
  // The leave fails, but the bot is not a member any more: that is the
  // outcome wanted.
  r = await run(new Set([g1]), false);
  assert.deepEqual([r.left, r.dropped], [["1"], true]);
  assert.deepEqual(tables(db), []);
});

test("an unknown creation with several matches, or when the groups cannot be listed, stays for the next start", async () => {
  const db = legacyDb([{ item: "7", jid: null, status: "unknown", token: "aaa" }]);
  const g = (n: string) => [`1203630000000000${n}`, "g.us"].join("@");
  const two = legacyApi({ groups: [{ jid: g("31"), subject: "#7 x ~aaa" }, { jid: g("32"), subject: "#7 y ~aaa" }] });
  let r = await cleanupLegacyGroups({ store: new IntakeStore(db), api: two.api, alertOperator: () => {}, log: quiet });
  assert.deepEqual([r.failed, r.dropped], [["7"], false]);
  assert.ok(!two.calls.some((c) => c.startsWith("leave")), "nothing is left on an ambiguous match");
  const broken: LegacyGroupApi = {
    leave: async () => {},
    listParticipating: async () => {
      throw new Error("offline");
    },
  };
  r = await cleanupLegacyGroups({ store: new IntakeStore(db), api: broken, alertOperator: () => {}, log: quiet });
  assert.deepEqual([r.failed, r.dropped], [["7"], false]);
  // No group carries the nonce: there is none to leave.
  r = await cleanupLegacyGroups({ store: new IntakeStore(db), api: legacyApi().api, alertOperator: () => {}, log: quiet });
  assert.deepEqual([r.left, r.failed, r.dropped], [[], [], true]);
});

test("an old creation with an unknown outcome keeps the request table until it is settled, and is reported after 3 starts", async () => {
  const db = legacyDb([{ item: "1", jid: null, status: "closed" }]);
  const raw = new DatabaseSync(db);
  // Item 8: claimed and sent to WhatsApp, then the bot stopped (no group
  // row). Item 9: claimed but never sent. Item 1: its group row is closed.
  raw.exec(`
    INSERT INTO intake_group_requests (item_key, action, enqueued_at, status, nonce, calling_at) VALUES ('8', 'create', 't', 'creating', 'n8', 't');
    INSERT INTO intake_group_requests (item_key, action, enqueued_at, status, nonce, calling_at) VALUES ('9', 'create', 't', 'creating', 'n9', NULL);
    INSERT INTO intake_group_requests (item_key, action, enqueued_at, status, nonce, calling_at) VALUES ('1', 'create', 't', 'creating', 'n1', 't');
  `);
  const alerts: Array<{ text: string; key: string }> = [];
  const offline: LegacyGroupApi = {
    leave: async () => {},
    listParticipating: async () => {
      throw new Error("offline");
    },
  };
  const run = (api: LegacyGroupApi) =>
    cleanupLegacyGroups({ store: new IntakeStore(db), api, alertOperator: (text, key) => alerts.push({ text, key }), log: quiet });
  let r = await run(offline);
  assert.deepEqual(r.kept, ["intake_group_requests"], "the requests stay while item 8's creation is unknown");
  assert.deepEqual(tables(db), ["intake_group_requests"], "the group table itself is dropped");
  r = await run(offline);
  assert.equal(alerts.length, 0, "not reported before the third start");
  r = await run(offline);
  assert.equal(alerts.length, 1);
  assert.match(alerts[0].text, /Item #8: an old WhatsApp group may have been created .* after 3 starts\. .*"~n8"/);
  assert.equal(alerts[0].key, "intake:group-request:2");
  // Found by its nonce and left: settled, the table goes.
  const g8 = ["120363000000000081", "g.us"].join("@");
  const { api, calls } = legacyApi({ groups: [{ jid: g8, subject: "#8 Fix ~n8" }] });
  r = await run(api);
  assert.deepEqual([r.left, r.kept, r.dropped], [["8"], [], true]);
  assert.deepEqual(calls, ["list", `leave ${g8}`]);
});

test("full intake messages queued by an earlier version are withdrawn at start and replaced by one notice per item", () => {
  const db = tmpDb();
  const store = new IntakeStore(db);
  const out = new OutboundQueueStore(db);
  const group = ["120363000000000091", "g.us"].join("@");
  const full = out.enqueue({ target: "dm", body: "[#4] A long agent reply with the whole plan…", source: "intake:4", dedupKey: "intake:4:m1" });
  const full2 = out.enqueue({ target: "dm", body: "[#4] (operator, via dashboard) JSON", source: "intake:4", dedupKey: "intake:4:m2" });
  const inFlight = out.enqueue({ target: "dm", body: "[#5] Plan v2 …", source: "intake:5", dedupKey: "intake:5:m3" });
  new DatabaseSync(db).prepare(`UPDATE outbound_queue SET status = 'in_flight', in_flight_at = ? WHERE id = ?`).run(new Date().toISOString(), inFlight);
  const toGroup = out.enqueue({ target: group, body: "Plan v1 of item 6", source: "intake:6", dedupKey: "intake:6:m4" });
  const notice = out.enqueue({ target: "dm", body: "🛠 Item #7: implementation started.", source: "intake:7", dedupKey: "intake:7:m5" });
  const ask = out.enqueue({ target: "dm", body: "Cancel item #7? Answer yes or no.", source: "intake:ask", dedupKey: "intake:answer:x" });
  const sent = out.enqueue({ target: "dm", body: "[#8] old", source: "intake:8", dedupKey: "intake:8:m6" });
  new DatabaseSync(db).prepare(`UPDATE outbound_queue SET status = 'sent' WHERE id = ?`).run(sent);
  const items = store.withdrawLegacyThreadMessages("Item #{n} has messages on the dashboard. {link}", "https://dash.example.invalid/");
  assert.deepEqual(items.sort(), ["4", "5", "6"]);
  const status = (id: number) =>
    (new DatabaseSync(db).prepare(`SELECT status FROM outbound_queue WHERE id = ?`).get(id) as { status: string }).status;
  for (const id of [full, full2, inFlight, toGroup]) assert.equal(status(id), "failed", `row ${id} is withdrawn`);
  for (const id of [notice, ask]) assert.equal(status(id), "pending", `row ${id} is a notice or a question and stays`);
  assert.equal(status(sent), "sent");
  const added = new DatabaseSync(db)
    .prepare(`SELECT target, body, source FROM outbound_queue WHERE dedup_key LIKE 'intake:withdrawn:%' ORDER BY source`)
    .all() as Array<{ target: string; body: string; source: string }>;
  assert.deepEqual(
    added.map((r) => ({ ...r })),
    ["4", "5", "6"].map((n) => ({ target: "dm", body: `Item #${n} has messages on the dashboard. https://dash.example.invalid/intake?item=${n}`, source: `intake:${n}` })),
  );
  // A second start withdraws nothing more and queues no second notice; with
  // no public URL the notice has no link.
  assert.deepEqual(store.withdrawLegacyThreadMessages("x", null), []);
  const db2 = tmpDb();
  new OutboundQueueStore(db2).enqueue({ target: "dm", body: "[#3] reply", source: "intake:3" });
  new IntakeStore(db2).withdrawLegacyThreadMessages("Item #{n} has messages on the dashboard. {link}", null);
  const body = (new DatabaseSync(db2).prepare(`SELECT body FROM outbound_queue WHERE dedup_key = 'intake:withdrawn:3'`).get() as { body: string }).body;
  assert.equal(body, "Item #3 has messages on the dashboard.");
});
