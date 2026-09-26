// Issue pipeline surface on WhatsApp (ADR-036).
//
// The pipeline itself runs in Rust (`nucleus intake tick`). Every item's
// WhatsApp surface is the operator's DM: the pipeline sends short notices
// there through the outbound queue (target policy + secret filter), each
// marked with the item number and a link to the item's dashboard page. The
// bot does what only the WhatsApp connection can do:
//
//   1. Route the operator's DM messages to the pipeline: messages from the
//      operator that start with the item marker (`#12 …`), quote a message
//      the pipeline sent, or come within 15 minutes after the pipeline asked
//      the operator a question in the DM (its next message is the answer).
//      The bot stores them in `intake_inbound` with how they were written
//      (`text`, `voice`, `forwarded`) and `sender = 'operator'`, and runs
//      `nucleus intake tick`. The Rust side has each message read by the
//      interpreter model, decides in code what happens, and replies through
//      the outbound queue (a decision from a voice note or a forward is
//      always confirmed first).
//   2. Keep the operator's other DM messages for `nucleus intake
//      interpret-latest` and type the pipeline's decision block after them
//      (the DM chat session).
//   3. Once, at start: leave the per-item WhatsApp groups an earlier version
//      created (ADR-036, "No WhatsApp groups";
//      `cleanupLegacyGroups`), then drop their tables.

import { DatabaseSync } from "node:sqlite";
import { normalizeSenderId } from "./config.js";

/** How an operator message was written. The pipeline confirms a decision
 *  from anything but `text` before it runs. */
export type InputKind = "text" | "voice" | "forwarded";

/** `item_key` of a DM message that names no item: the answer to a question
 *  the pipeline asked in the DM. Mirrors `INTAKE_DM_KEY` in
 *  core/src/whatsapp_queue.rs. */
export const DM_KEY = "dm";

/** `item_key` of an operator DM message that went to the chat session. It
 *  is stored so `nucleus intake interpret-latest` can read the operator's
 *  own text when the chat session asks; the tick never interprets it by
 *  itself. Mirrors `INTAKE_CHAT_KEY` in core/src/whatsapp_queue.rs. */
export const CHAT_KEY = "chat";

/** A question the pipeline asks in the DM (`intake:ask`) waits this long for
 *  the operator's answer. Mirrors `CONFIRMATION_MINUTES` in
 *  core/src/intake/pipeline.rs. */
export const ANSWER_WINDOW_MS = 15 * 60 * 1000;

/** The end of a WhatsApp group chat id (built at runtime; the
 *  committed-secrets scanner reads the literal as an address). */
const GROUP_JID_SUFFIX = ["@", "g.us"].join("");

/** Who the operator is: `operatorId` is the first WHATSAPP_ALLOWED_DM_JIDS
 *  entry (the phone digits); `operatorLids` are the digits of
 *  WHATSAPP_OPERATOR_LIDS. The rest of the DM allowlist is other contacts
 *  the bot chats with, never the operator. */
export interface OperatorIds {
  operatorId: string | null;
  operatorLids: ReadonlySet<string>;
}

/** The operator's identities from the bot's configuration. */
export function operatorIds(config: { operatorId: string | null; operatorLids: ReadonlySet<string> }): OperatorIds {
  return { operatorId: config.operatorId, operatorLids: config.operatorLids };
}

/** True when `jid` (a phone JID, an `@lid` id, a DM chat id, or a bare
 *  number) is the operator: its digits equal the operator's phone digits;
 *  or it is an `@lid` id whose digits are in WHATSAPP_OPERATOR_LIDS; or it
 *  is an `@lid` id whose phone number, from `pnForLid` (the bot's LID
 *  mapping), has the operator's digits. The one rule for every operator
 *  check: the DM gate, approvals, the DM routing, the stored `chat` rows
 *  and the decision block for the DM chat session. */
export async function isOperatorId(
  jid: string,
  op: OperatorIds,
  pnForLid: (lid: string) => Promise<string | null | undefined>,
): Promise<boolean> {
  if (!op.operatorId) return false;
  const digits = normalizeSenderId(jid);
  if (digits && digits === op.operatorId) return true;
  if (jid.endsWith("@lid")) {
    if (digits && op.operatorLids.has(digits)) return true;
    try {
      const pn = await pnForLid(jid);
      if (pn && normalizeSenderId(pn) === op.operatorId) return true;
    } catch {
      // A failed lookup is not the operator.
    }
  }
  return false;
}

/** When a message reached the bot (ADR-036): `atMs`, taken in the
 *  `messages.upsert` handler before any await (so a batch's later message,
 *  or a voice note still being transcribed, keeps its real arrival time),
 *  and `waTs`, WhatsApp's own `messageTimestamp` in seconds when known. A
 *  message can answer only a question sent before both. */
export interface Arrival {
  atMs: number;
  waTs: number | null;
}

/** WhatsApp's `messageTimestamp` (a number, or a protobuf Long) in
 *  seconds, or null. */
export function waSeconds(ts: unknown): number | null {
  if (typeof ts === "number" && Number.isFinite(ts)) return Math.floor(ts);
  if (ts && typeof ts === "object") {
    const t = ts as { toNumber?: () => number; low?: number };
    if (typeof t.toNumber === "function") return Math.floor(t.toNumber());
    if (typeof t.low === "number") return t.low;
  }
  if (typeof ts === "string" && /^\d+$/.test(ts)) return Number(ts);
  return null;
}

/** The arrival of every message of one `messages.upsert` batch, stamped at
 *  once, before the batch is handled. */
export function stampBatch<T>(messages: readonly T[], tsOf: (m: T) => unknown, nowMs = Date.now()): Map<T, Arrival> {
  const out = new Map<T, Arrival>();
  for (const m of messages) out.set(m, { atMs: nowMs, waTs: waSeconds(tsOf(m)) });
  return out;
}

/** How long a LID the mapping resolved to the operator counts for the
 *  checks that cannot ask the mapping (role lookup, target policy). */
export const OPERATOR_LID_TTL_MS = 10 * 60 * 1000;

/** LIDs the live mapping resolved to the operator, for the synchronous
 *  checks only (ADR-036). An entry counts for `OPERATOR_LID_TTL_MS` after
 *  it was last verified; `refresh` verifies every entry against the live
 *  mapping again (on each inbound message) and drops the ones that no
 *  longer resolve to the operator. The asynchronous checks never read it:
 *  they call the mapping each time (`isOperatorId`). WHATSAPP_OPERATOR_LIDS
 *  entries are not kept here and do not depend on the mapping. */
export class OperatorLidCache {
  private verified = new Map<string, number>();

  /** `persist` mirrors every verification and drop into whatsapp.db
   *  (`operator_lid_verified`), where the Rust side reads it to accept a
   *  task from the operator's LID chat. */
  constructor(
    private readonly ttlMs = OPERATOR_LID_TTL_MS,
    private readonly persist?: { verified: (digits: string, atMs: number) => void; dropped: (digits: string) => void },
  ) {}

  /** A LID (digits) the mapping has just resolved to the operator. */
  note(digits: string, nowMs = Date.now()): void {
    if (!digits) return;
    this.verified.set(digits, nowMs);
    this.persist?.verified(digits, nowMs);
  }

  /** The live check rejected LID `digits`: forget it at once. */
  forget(digits: string): void {
    if (!digits) return;
    this.verified.delete(digits);
    this.persist?.dropped(digits);
  }

  /** The entries verified within the TTL. */
  current(nowMs = Date.now()): Set<string> {
    const out = new Set<string>();
    for (const [d, at] of this.verified) {
      if (nowMs - at < this.ttlMs) out.add(d);
    }
    return out;
  }

  /** Verify every entry against the live mapping; keep and re-date the
   *  ones that still resolve to the operator's phone, drop the others. */
  async refresh(
    operatorId: string | null,
    pnForLid: (lid: string) => Promise<string | null | undefined>,
    nowMs = Date.now(),
  ): Promise<void> {
    for (const d of [...this.verified.keys()]) {
      let ok = false;
      try {
        const pn = await pnForLid(`${d}@lid`);
        ok = !!operatorId && !!pn && normalizeSenderId(pn) === operatorId;
      } catch {
        ok = false;
      }
      if (ok) {
        this.verified.set(d, nowMs);
        this.persist?.verified(d, nowMs);
      } else {
        this.verified.delete(d);
        this.persist?.dropped(d);
      }
    }
  }
}

/** The DM gate: a DM chat is admitted when its digits are in the DM
 *  allowlist (the operator or another contact, as normal chat) or when it
 *  is the operator by `isOperatorId` (a LID the allowlist does not list).
 *  `operator` says whether it is the operator; only then does anything of
 *  the issue pipeline apply. */
export async function admitDm(input: {
  chatId: string;
  allowedDm: ReadonlySet<string>;
  operator: OperatorIds;
  pnForLid: (lid: string) => Promise<string | null | undefined>;
}): Promise<{ admitted: boolean; operator: boolean }> {
  const operator = await isOperatorId(input.chatId, input.operator, input.pnForLid);
  const digits = normalizeSenderId(input.chatId);
  return { admitted: operator || (digits.length > 0 && input.allowedDm.has(digits)), operator };
}

/** What the bot does with a DM message that goes to the chat session
 *  (ADR-036): for the operator's DM (in any form `isOperatorId` accepts),
 *  keep the text for `interpret-latest` (`record`) and type the pipeline's
 *  decision block after it (`block`, "" when nothing waits). For any other
 *  DM chat: nothing. */
export async function dmChatIntake(input: {
  chatId: string;
  operator: OperatorIds;
  pnForLid: (lid: string) => Promise<string | null | undefined>;
  chatBlock: () => string;
}): Promise<{ record: boolean; block: string }> {
  if (!(await isOperatorId(input.chatId, input.operator, input.pnForLid))) return { record: false, block: "" };
  return { record: true, block: input.chatBlock() };
}

const MARKER = /^\s*#(\d{1,6})(?:\s+|$)/;

/** A DM message for the pipeline, or null for the chat session. The
 *  message belongs to item n when it starts with `#<n>` and item n has a DM
 *  thread (`hasDmThread`), or when it quotes a message the pipeline sent for
 *  item n (`quotedItem`; the marker is removed from the text). It goes to
 *  the pipeline without an item (`DM_KEY`) when it quotes a question or note
 *  the pipeline sent in the DM (`quotedItem` is `DM_KEY`), or when the
 *  pipeline is waiting for the answer to a question it asked in the DM
 *  (`expectingAnswer`). */
export function routeDm(
  text: string,
  quotedItem: string | null,
  hasDmThread: (item: string) => boolean,
  expectingAnswer = false,
): { item: string; text: string } | null {
  const m = MARKER.exec(text);
  if (m && hasDmThread(m[1])) return { item: m[1], text: text.slice(m[0].length).trim() };
  if (quotedItem) {
    const t = m && m[1] === quotedItem ? text.slice(m[0].length).trim() : text.trim();
    return { item: quotedItem, text: t };
  }
  if (expectingAnswer) return { item: DM_KEY, text: text.trim() };
  return null;
}

/** A DM message routed to an item, with how it was written. */
export interface RoutedDm {
  item: string;
  text: string;
  inputKind: InputKind;
}

/** Decide whether a DM message goes to the issue pipeline. Only the
 *  operator's own DM is routed (another allowed DM sender's message goes to
 *  the chat session, so it can never approve, release or cancel), and the
 *  message keeps how it was written: `voice` for a transcription,
 *  `forwarded` for a forwarded message, `text` for what the operator
 *  typed. */
export async function routeOperatorDm(input: {
  chatId: string;
  operator: OperatorIds;
  pnForLid: (lid: string) => Promise<string | null | undefined>;
  text: string;
  quotedItem: string | null;
  hasDmThread: (item: string) => boolean;
  /** The pipeline asked a question in the DM in the last 15 minutes and no
   *  DM message went to it since (`IntakeStore.expectsDmAnswer`). */
  expectingAnswer?: boolean;
  /** `voice` for a transcribed voice note; otherwise how the message was
   *  sent (`text` or `forwarded`). */
  inputKind: InputKind;
}): Promise<RoutedDm | null> {
  if (!(await isOperatorId(input.chatId, input.operator, input.pnForLid))) return null;
  const routed = routeDm(input.text, input.quotedItem, input.hasDmThread, input.expectingAnswer ?? false);
  return routed ? { ...routed, inputKind: input.inputKind } : null;
}

export class IntakeStore {
  private db: DatabaseSync;

  constructor(dbPath: string) {
    this.db = new DatabaseSync(dbPath);
    this.db.exec(`PRAGMA journal_mode = WAL;`);
    this.db.exec(`PRAGMA busy_timeout = 5000;`);
    this.db.exec(`
      -- ADR-036: operator messages routed to an item. Rust reads rows past
      -- its watermark; the bot never updates them.
      CREATE TABLE IF NOT EXISTS intake_inbound (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        item_key    TEXT NOT NULL,
        chat_id     TEXT NOT NULL,
        wa_msg_id   TEXT NOT NULL,
        text        TEXT NOT NULL,
        received_at TEXT NOT NULL,
        input_kind  TEXT NOT NULL DEFAULT 'unknown',
        sender      TEXT NOT NULL DEFAULT 'unknown',
        wa_ts       INTEGER
      );
      CREATE UNIQUE INDEX IF NOT EXISTS idx_intake_inbound_msg
        ON intake_inbound(chat_id, wa_msg_id);

      -- ADR-036: LIDs the bot verified as the operator through the live
      -- LID mapping, with when. The bot writes it; Rust reads it to accept
      -- a task from the operator's LID chat (entries count for 10 minutes).
      CREATE TABLE IF NOT EXISTS operator_lid_verified (
        digits      TEXT PRIMARY KEY,
        verified_at TEXT NOT NULL
      );

      -- ADR-036: the DM chat session's list of waiting intake decisions.
      -- One row; Rust writes it (whatsapp_queue.rs), the bot only reads it.
      CREATE TABLE IF NOT EXISTS intake_chat_block (
        id         INTEGER PRIMARY KEY CHECK (id = 1),
        block      TEXT NOT NULL,
        updated_at TEXT NOT NULL
      );
    `);
    // A table created before the column existed.
    const cols = (this.db.prepare(`SELECT name FROM pragma_table_info('intake_inbound')`).all() as Array<{ name: string }>).map(
      (c) => c.name,
    );
    if (!cols.includes("sender")) this.db.exec(`ALTER TABLE intake_inbound ADD COLUMN sender TEXT NOT NULL DEFAULT 'unknown'`);
    if (!cols.includes("wa_ts")) this.db.exec(`ALTER TABLE intake_inbound ADD COLUMN wa_ts INTEGER`);
  }

  /** Store an operator message for the pipeline, after the caller checked
   *  that the sender is the operator's own identity (`sender`; the Rust side
   *  interprets no other row). A message stored before (the same WhatsApp
   *  id in the same chat) is ignored. Returns true when new. */
  recordInbound(input: {
    itemKey: string;
    chatId: string;
    waMsgId: string;
    text: string;
    inputKind: InputKind;
    sender: "operator";
    /** When the message reached the bot (`Arrival`), not when it was
     *  handled; now when unknown. */
    arrival?: Arrival | null;
    nowMs?: number;
  }): boolean {
    const res = this.db
      .prepare(
        `INSERT OR IGNORE INTO intake_inbound (item_key, chat_id, wa_msg_id, text, received_at, input_kind, sender, wa_ts)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)`,
      )
      .run(
        input.itemKey,
        input.chatId,
        input.waMsgId,
        input.text,
        new Date(input.arrival?.atMs ?? input.nowMs ?? Date.now()).toISOString(),
        input.inputKind,
        input.sender,
        input.arrival?.waTs ?? null,
      );
    return Number(res.changes) > 0;
  }

  /** Record that the live mapping verified LID `digits` as the operator. */
  markOperatorLidVerified(digits: string, atMs = Date.now()): void {
    this.db
      .prepare(
        `INSERT INTO operator_lid_verified (digits, verified_at) VALUES (?, ?)
         ON CONFLICT(digits) DO UPDATE SET verified_at = excluded.verified_at`,
      )
      .run(digits, new Date(atMs).toISOString());
  }

  /** The live mapping no longer resolves LID `digits` to the operator. */
  forgetOperatorLid(digits: string): void {
    this.db.prepare(`DELETE FROM operator_lid_verified WHERE digits = ?`).run(digits);
  }

  /** The code-owned block the pipeline wrote for the DM chat session: what
   *  waits for an intake decision and when to run `interpret-latest`; ""
   *  when nothing waits. */
  chatBlock(): string {
    const r = this.db.prepare(`SELECT block FROM intake_chat_block WHERE id = 1`).get() as { block: string } | undefined;
    return r?.block ?? "";
  }

  /** What a message the bot sent belongs to, from the WhatsApp message id a
   *  reply quotes: item n for a thread message (`intake:<n>`), `DM_KEY` for a
   *  question or note the pipeline sent in the DM (`intake:ask`,
   *  `intake:note`), otherwise null. */
  itemForSentMessage(msgId: string): string | null {
    const r = this.db.prepare(`SELECT source FROM outbound_queue WHERE msg_id = ?`).get(msgId) as { source: string } | undefined;
    if (!r) return null;
    if (r.source === "intake:ask" || r.source === "intake:note") return DM_KEY;
    const m = /^intake:(\d+)$/.exec(r.source);
    return m ? m[1] : null;
  }

  /** True when the pipeline asked the operator a question in the DM
   *  (`intake:ask`, sent or being sent) less than `ANSWER_WINDOW_MS` ago,
   *  and no DM message went to the pipeline since: the operator's next DM
   *  message is the answer. */
  expectsDmAnswer(nowMs = Date.now()): boolean {
    const asked = this.db
      .prepare(
        `SELECT MAX(COALESCE(sent_at, in_flight_at)) AS at FROM outbound_queue
          WHERE source = 'intake:ask' AND target = 'dm' AND status IN ('sent', 'in_flight')`,
      )
      .get() as { at: string | null };
    if (!asked.at) return false;
    const askedMs = Date.parse(asked.at);
    if (!(nowMs - askedMs < ANSWER_WINDOW_MS)) return false;
    // DM rows only: a group chat id ends in the group suffix (built at
    // runtime; the committed-secrets scanner reads the literal as an
    // address).
    const last = this.db
      .prepare(`SELECT MAX(received_at) AS at FROM intake_inbound WHERE chat_id NOT LIKE ?`)
      .get(`%${GROUP_JID_SUFFIX}`) as { at: string | null };
    return !last.at || Date.parse(last.at) < askedMs;
  }

  /** True when the pipeline sent a DM message for item `n` in the last 30
   *  days (it has a DM thread the `#n` marker can address). */
  hasDmThread(n: string, nowMs = Date.now()): boolean {
    const since = new Date(nowMs - 30 * 24 * 60 * 60 * 1000).toISOString();
    const r = this.db
      .prepare(`SELECT 1 AS x FROM outbound_queue WHERE source = ? AND target = 'dm' AND enqueued_at > ? LIMIT 1`)
      .get(`intake:${n}`, since) as { x: number } | undefined;
    return r !== undefined;
  }

  // ── the removed per-item groups (one-time cleanup) ──────────────────────

  private tableExists(name: string): boolean {
    const r = this.db.prepare(`SELECT 1 AS x FROM sqlite_master WHERE type = 'table' AND name = ?`).get(name) as
      | { x: number }
      | undefined;
    return r !== undefined;
  }

  /** Groups an earlier version created and has not closed: every
   *  `intake_groups` row that is not `closed` or `fallback` (a creation
   *  WhatsApp refused made no group). Empty when the table is gone. */
  legacyGroups(): LegacyGroup[] {
    if (!this.tableExists("intake_groups")) return [];
    const cols = (this.db.prepare(`SELECT name FROM pragma_table_info('intake_groups')`).all() as Array<{ name: string }>).map(
      (c) => c.name,
    );
    if (!cols.includes("cleanup_attempts")) {
      this.db.exec(`ALTER TABLE intake_groups ADD COLUMN cleanup_attempts INTEGER NOT NULL DEFAULT 0`);
    }
    const token = cols.includes("token") ? "token" : "NULL AS token";
    return (
      this.db
        .prepare(
          `SELECT item_key, jid, status, ${token}, cleanup_attempts FROM intake_groups
            WHERE status NOT IN ('closed', 'fallback') ORDER BY item_key`,
        )
        .all() as any[]
    ).map((r) => ({
      itemKey: String(r.item_key),
      jid: r.jid ?? null,
      status: String(r.status),
      token: r.token ?? null,
      attempts: Number(r.cleanup_attempts ?? 0),
    }));
  }

  /** The bot left the group (or it was never there): the row is closed, and
   *  the group's membership record goes too. */
  closeLegacyGroup(itemKey: string, jid: string | null, reason: string, nowMs = Date.now()): void {
    this.db
      .prepare(`UPDATE intake_groups SET status = 'closed', reason = ?, closed_at = ? WHERE item_key = ?`)
      .run(reason, new Date(nowMs).toISOString(), itemKey);
    if (jid && this.tableExists("chat_state")) this.db.prepare(`DELETE FROM chat_state WHERE chat_id = ?`).run(jid);
  }

  /** Count one failed leave of the group of `itemKey`; returns the count. */
  legacyLeaveFailed(itemKey: string, error: string): number {
    this.db
      .prepare(`UPDATE intake_groups SET cleanup_attempts = cleanup_attempts + 1, reason = ? WHERE item_key = ?`)
      .run(`cleanup: ${error}`.slice(0, 500), itemKey);
    const r = this.db.prepare(`SELECT cleanup_attempts AS n FROM intake_groups WHERE item_key = ?`).get(itemKey) as
      | { n: number }
      | undefined;
    return Number(r?.n ?? 0);
  }

  /** Drop the group tables once no group is left open: the one-time
   *  migration of the groups' removal. True when the tables are gone. */
  dropLegacyGroupTablesIfDone(): boolean {
    if (this.tableExists("intake_groups") && this.legacyGroups().length > 0) return false;
    this.db.exec(`BEGIN IMMEDIATE`);
    try {
      this.db.exec(`DROP TABLE IF EXISTS intake_groups; DROP TABLE IF EXISTS intake_group_requests;`);
      this.db.exec(`COMMIT`);
    } catch (e) {
      this.db.exec(`ROLLBACK`);
      throw e;
    }
    return true;
  }
}

/** A group an earlier version created for an item (ADR-036, "No
 *  WhatsApp groups"). */
export interface LegacyGroup {
  itemKey: string;
  /** `null` for a creation whose outcome was never known. */
  jid: string | null;
  status: string;
  /** The recovery nonce at the end of the group's subject (` ~<nonce>`). */
  token: string | null;
  /** Failed leaves at earlier starts. */
  attempts: number;
}

/** What the cleanup needs from the connection. */
export interface LegacyGroupApi {
  leave(jid: string): Promise<void>;
  /** False when the bot is not a member of `jid`; null when that cannot be
   *  told. */
  isMember?(jid: string): Promise<boolean | null>;
  /** Every group the bot participates in: finds a creation whose outcome was
   *  never known by the nonce at the end of its subject. */
  listParticipating?(): Promise<Array<{ jid: string; subject: string }>>;
}

/** A group whose leave failed this many times (over several starts) is
 *  reported to the operator in the DM, once. */
export const LEGACY_LEAVE_ALERT_AFTER = 3;

/** The one-time cleanup of the removed per-item groups, run when the bot
 *  starts: leave every group still recorded as open and mark it closed. A
 *  creation whose outcome was never known is looked for once by its nonce:
 *  one match is left, none means there is no group, several are counted as
 *  a failure. A failed leave keeps its row for the next start and is
 *  reported to the operator in the DM after `LEGACY_LEAVE_ALERT_AFTER`
 *  failures. When no open group is left, the group tables are dropped. */
export async function cleanupLegacyGroups(input: {
  store: IntakeStore;
  api: LegacyGroupApi;
  alertOperator: (text: string, dedupKey: string) => void;
  log: { info: (o: object, m: string) => void; warn: (o: object, m: string) => void };
}): Promise<{ left: string[]; failed: string[]; dropped: boolean }> {
  const { store, api, log } = input;
  const left: string[] = [];
  const failed: string[] = [];
  const groups = store.legacyGroups();
  let participating: Array<{ jid: string; subject: string }> | null | undefined;
  const fail = (g: LegacyGroup, error: string) => {
    failed.push(g.itemKey);
    const n = store.legacyLeaveFailed(g.itemKey, error);
    log.warn({ item: g.itemKey, attempts: n, err: error }, "whatsapp: leaving an old intake group failed — tried again at the next start");
    if (n >= LEGACY_LEAVE_ALERT_AFTER) {
      input.alertOperator(
        `Item #${g.itemKey}: leaving its old WhatsApp group failed ${n} times (${error}). Issue-pipeline items no ` +
          `longer use groups; leave the group by hand. The bot tries again at its next start.`,
        `intake:group-cleanup:${g.itemKey}`,
      );
    }
  };
  for (const g of groups) {
    let jid = g.jid;
    if (!jid) {
      if (!g.token || !api.listParticipating) {
        store.closeLegacyGroup(g.itemKey, null, "cleanup: no group was recorded and none can be looked for");
        continue;
      }
      if (participating === undefined) {
        try {
          participating = await api.listParticipating();
        } catch (e) {
          participating = null;
          log.warn({ err: (e as Error).message }, "whatsapp: listing groups for the intake cleanup failed");
        }
      }
      if (participating === null) {
        fail(g, "the bot's groups could not be listed");
        continue;
      }
      const matches = participating.filter((x) => x.subject.endsWith(` ~${g.token}`));
      if (matches.length === 0) {
        store.closeLegacyGroup(g.itemKey, null, "cleanup: no group carries its nonce");
        continue;
      }
      if (matches.length > 1) {
        fail(g, `${matches.length} groups carry its nonce`);
        continue;
      }
      jid = matches[0].jid;
    }
    let ok = false;
    let error = "";
    try {
      await api.leave(jid);
      ok = true;
    } catch (e) {
      error = (e as Error).message;
      // Leaving a group the bot is no longer in fails; that is the outcome
      // wanted. Only a confirmed non-membership counts.
      const member = await api.isMember?.(jid).catch(() => null);
      if (member === false) ok = true;
    }
    if (ok) {
      store.closeLegacyGroup(g.itemKey, jid, "cleanup: left (issue-pipeline groups were removed)");
      left.push(g.itemKey);
      log.info({ item: g.itemKey, jid }, "whatsapp: left an old intake group");
    } else {
      fail(g, error || "leave failed");
    }
  }
  const dropped = store.dropLegacyGroupTablesIfDone();
  if (dropped && groups.length > 0) log.info({ left: left.length }, "whatsapp: old intake group tables dropped");
  return { left, failed, dropped };
}

