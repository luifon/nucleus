// Issue pipeline surface on WhatsApp (ADR-036).
//
// The pipeline itself runs in Rust (`nucleus intake tick`). The bot does the
// three things only the WhatsApp connection can do:
//
//   1. Create and leave an item's WhatsApp group. The pipeline queues a
//      request in `intake_group_requests` (a queue table: Rust inserts, the
//      bot drains); the bot records the outcome in its own `intake_groups`
//      table, which Rust reads. A created group contains the bot and the
//      operator only; its membership baseline is the create response.
//   2. Route the operator's messages to an item: messages from the operator
//      (the first WHATSAPP_ALLOWED_DM_JIDS entry, phone or LID form) in an
//      item's group, and operator DM messages that start with the item
//      marker (`#12 …`) or quote a message the pipeline sent for that item.
//      The bot stores them in `intake_inbound` with how they were written
//      (`text`, `voice`, `forwarded`: only typed text can be a command) and
//      runs `nucleus intake tick`; the Rust side reads them, decides
//      approvals, and replies through the outbound queue.
//   3. Keep the intake groups in the target allowlist while they are
//      active, so the outbound drain (target policy + secret filter) can
//      send to them, and remove them when the bot has left.
//
// Request handling: a request is claimed with a conditional update
// (`pending` → `creating` / `closing`) before any WhatsApp call, so two
// passes never act on one request. A creation whose outcome is unknown (the
// bot stopped mid-call) is never retried: it is recorded as `unknown` and the
// operator is told. Leaving is retried with backoff until the bot confirms it
// left; `closed_at` is set only then. A group created for an item that was
// closed meanwhile is left at once.
//
// Group creation is rate-limited twice: the pipeline requests at most
// `[intake.whatsapp] max_groups_per_day` groups in 24 hours, and the bot
// refuses to create more than that itself (creations with an unknown outcome
// count). Automated group creation from a personal account can trigger
// WhatsApp's anti-spam checks; a refused request is recorded as `fallback`
// and the item's thread runs in the DM.

import { DatabaseSync } from "node:sqlite";
import { normalizeSenderId } from "./config.js";

/** A group request the pipeline queued. */
export interface GroupRequest {
  id: number;
  itemKey: string;
  action: "create" | "close";
  subject: string | null;
  enqueuedAt: string;
  attempts: number;
}

export interface IntakeGroupRow {
  itemKey: string;
  jid: string | null;
  /** `unknown`: a creation whose outcome is not known (the group may exist). */
  status: "active" | "fallback" | "closed" | "unknown";
  reason: string | null;
  createdAt: string;
}

/** How an operator message was written. Only `text` can be a command. */
export type InputKind = "text" | "voice" | "forwarded";

/** True when `jid` (a phone JID, an `@lid` id, or a bare number) is the
 *  operator: its digits equal `operatorId`, or it is a LID whose phone
 *  number (from `pnForLid`, the bot's LID mapping) has those digits. The
 *  same rule the group sender check uses. */
export async function isOperatorId(
  jid: string,
  operatorId: string | null,
  pnForLid: (lid: string) => Promise<string | null | undefined>,
): Promise<boolean> {
  if (!operatorId) return false;
  const digits = normalizeSenderId(jid);
  if (digits && digits === operatorId) return true;
  if (jid.endsWith("@lid")) {
    try {
      const pn = await pnForLid(jid);
      if (pn && normalizeSenderId(pn) === operatorId) return true;
    } catch {
      // A failed lookup is not the operator.
    }
  }
  return false;
}

/** Members of a new group that are neither the bot nor the operator. */
export async function unexpectedMembers(
  members: readonly string[],
  selfIds: readonly string[],
  isOperator: (jid: string) => Promise<boolean>,
): Promise<string[]> {
  const self = new Set(selfIds.map(normalizeSenderId).filter((s) => s.length > 0));
  const out: string[] = [];
  for (const m of members) {
    if (self.has(normalizeSenderId(m))) continue;
    if (await isOperator(m)) continue;
    out.push(m);
  }
  return out;
}

/** Delay before retry `attempts` (1-based) of leaving a group: 30 s,
 *  doubling, at most one hour. */
export function closeBackoffMs(attempts: number): number {
  return Math.min(30_000 * 2 ** Math.max(0, attempts - 1), 60 * 60 * 1000);
}

/** Leaving a group is given up (and the operator told) after this many
 *  failed attempts. */
export const MAX_CLOSE_ATTEMPTS = 8;
/** A claim (`creating` / `closing`) older than this was left by a stopped
 *  bot. */
const STUCK_CLAIM_MS = 10 * 60 * 1000;

export interface IntakeWhatsAppConfig {
  refinementGroups: boolean;
  maxGroupsPerDay: number;
}

export function intakeConfig(t: Record<string, unknown> = {}): IntakeWhatsAppConfig {
  const max = typeof t.max_groups_per_day === "number" && t.max_groups_per_day >= 0 ? t.max_groups_per_day : 3;
  return { refinementGroups: t.refinement_groups !== false, maxGroupsPerDay: max };
}

/** True when one more group may be created: fewer than `maxPerDay`
 *  creations in the 24 hours before `nowMs`. Mirrors
 *  `group_budget_allows` in core/src/intake/stage.rs. */
export function groupBudgetAllows(createdAt: readonly string[], nowMs: number, maxPerDay: number): boolean {
  if (maxPerDay <= 0) return false;
  const since = nowMs - 24 * 60 * 60 * 1000;
  return createdAt.filter((t) => Date.parse(t) > since).length < maxPerDay;
}

const MARKER = /^\s*#(\d{1,6})(?:\s+|$)/;

/** A DM message for an item, or null for the chat session. The message
 *  belongs to an item when it quotes a message the pipeline sent for it
 *  (`quotedItem`), or when it starts with `#<n>` and item n has a DM
 *  thread (`hasDmThread`). The marker is removed from the text. */
export function routeDm(
  text: string,
  quotedItem: string | null,
  hasDmThread: (item: string) => boolean,
): { item: string; text: string } | null {
  const m = MARKER.exec(text);
  if (m && hasDmThread(m[1])) return { item: m[1], text: text.slice(m[0].length).trim() };
  if (quotedItem) {
    const t = m && m[1] === quotedItem ? text.slice(m[0].length).trim() : text.trim();
    return { item: quotedItem, text: t };
  }
  return null;
}

/** In an item's group the `#<n>` marker is optional; remove it when it
 *  names that item. */
export function stripGroupMarker(text: string, itemKey: string): string {
  const m = MARKER.exec(text);
  return m && m[1] === itemKey ? text.slice(m[0].length).trim() : text.trim();
}

export class IntakeStore {
  private db: DatabaseSync;

  constructor(dbPath: string) {
    this.db = new DatabaseSync(dbPath);
    this.db.exec(`PRAGMA journal_mode = WAL;`);
    this.db.exec(`PRAGMA busy_timeout = 5000;`);
    this.db.exec(`
      -- ADR-036 queue table: the pipeline (Rust, nucleus_core::whatsapp_queue)
      -- inserts; the bot drains. Must match whatsapp_queue.rs.
      CREATE TABLE IF NOT EXISTS intake_group_requests (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        item_key    TEXT    NOT NULL,
        action      TEXT    NOT NULL,
        subject     TEXT,
        enqueued_at TEXT    NOT NULL,
        status      TEXT    NOT NULL DEFAULT 'pending',
        result      TEXT,
        handled_at  TEXT,
        dedup_key   TEXT,
        attempts    INTEGER NOT NULL DEFAULT 0,
        next_attempt_at TEXT,
        claimed_at  TEXT
      );
      CREATE UNIQUE INDEX IF NOT EXISTS idx_intake_group_requests_dedup
        ON intake_group_requests(dedup_key) WHERE dedup_key IS NOT NULL;

      -- ADR-036: the bot's record of each item's group. Rust reads it.
      CREATE TABLE IF NOT EXISTS intake_groups (
        item_key   TEXT PRIMARY KEY,
        jid        TEXT,
        subject    TEXT,
        status     TEXT NOT NULL,
        reason     TEXT,
        created_at TEXT NOT NULL,
        closed_at  TEXT,
        members_json TEXT
      );
      CREATE UNIQUE INDEX IF NOT EXISTS idx_intake_groups_jid
        ON intake_groups(jid) WHERE jid IS NOT NULL;

      -- ADR-036: operator messages routed to an item. Rust reads rows past
      -- its watermark; the bot never updates them.
      CREATE TABLE IF NOT EXISTS intake_inbound (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        item_key    TEXT NOT NULL,
        chat_id     TEXT NOT NULL,
        wa_msg_id   TEXT NOT NULL,
        text        TEXT NOT NULL,
        received_at TEXT NOT NULL,
        input_kind  TEXT NOT NULL DEFAULT 'unknown'
      );
      CREATE UNIQUE INDEX IF NOT EXISTS idx_intake_inbound_msg
        ON intake_inbound(chat_id, wa_msg_id);
    `);
    // Columns added after the first release of these tables (a table the
    // pipeline or an older bot created).
    this.addColumn("intake_group_requests", "attempts", "INTEGER NOT NULL DEFAULT 0");
    this.addColumn("intake_group_requests", "next_attempt_at", "TEXT");
    this.addColumn("intake_group_requests", "claimed_at", "TEXT");
    this.addColumn("intake_groups", "members_json", "TEXT");
    this.addColumn("intake_inbound", "input_kind", "TEXT NOT NULL DEFAULT 'unknown'");
  }

  private addColumn(table: string, column: string, decl: string): void {
    const cols = this.db.prepare(`SELECT name FROM pragma_table_info(?)`).all(table) as Array<{ name: string }>;
    if (!cols.some((c) => c.name === column)) this.db.exec(`ALTER TABLE ${table} ADD COLUMN ${column} ${decl}`);
  }

  /** Pending requests that are due (a close waiting for its backoff is
   *  not), oldest first. */
  pendingRequests(limit = 10, nowMs = Date.now()): GroupRequest[] {
    const now = new Date(nowMs).toISOString();
    return (
      this.db
        .prepare(
          `SELECT id, item_key, action, subject, enqueued_at, attempts FROM intake_group_requests
            WHERE status = 'pending' AND (next_attempt_at IS NULL OR next_attempt_at <= ?) ORDER BY id LIMIT ?`,
        )
        .all(now, limit) as any[]
    ).map((r) => ({
      id: r.id,
      itemKey: r.item_key,
      action: r.action,
      subject: r.subject,
      enqueuedAt: r.enqueued_at,
      attempts: Number(r.attempts ?? 0),
    }));
  }

  /** Claim a pending request for this pass (`creating` or `closing`).
   *  False when another pass claimed it first or it is no longer pending. */
  claim(id: number, as: "creating" | "closing", nowMs = Date.now()): boolean {
    const res = this.db
      .prepare(`UPDATE intake_group_requests SET status = ?, claimed_at = ? WHERE id = ? AND status = 'pending'`)
      .run(as, new Date(nowMs).toISOString(), id);
    return Number(res.changes) === 1;
  }

  finishRequest(id: number, status: "done" | "failed", result: string): void {
    this.db
      .prepare(`UPDATE intake_group_requests SET status = ?, result = ?, handled_at = ? WHERE id = ?`)
      .run(status, result, new Date().toISOString(), id);
  }

  /** A claimed close that failed goes back to `pending`, due after the
   *  backoff. */
  retryLater(id: number, attempts: number, result: string, nowMs = Date.now()): void {
    this.db
      .prepare(
        `UPDATE intake_group_requests SET status = 'pending', attempts = ?, result = ?, next_attempt_at = ?, claimed_at = NULL
          WHERE id = ?`,
      )
      .run(attempts, result, new Date(nowMs + closeBackoffMs(attempts)).toISOString(), id);
  }

  /** True when a close request for the item is pending or being handled
   *  (the item was closed). */
  closeRequested(itemKey: string): boolean {
    const r = this.db
      .prepare(
        `SELECT 1 AS x FROM intake_group_requests WHERE item_key = ? AND action = 'close' AND status IN ('pending', 'closing') LIMIT 1`,
      )
      .get(itemKey) as { x: number } | undefined;
    return r !== undefined;
  }

  /** Claims older than `STUCK_CLAIM_MS`: left by a bot that stopped. */
  stuckClaims(nowMs = Date.now()): Array<{ id: number; itemKey: string; action: string; subject: string | null }> {
    const before = new Date(nowMs - STUCK_CLAIM_MS).toISOString();
    return (
      this.db
        .prepare(
          `SELECT id, item_key, action, subject FROM intake_group_requests
            WHERE status IN ('creating', 'closing') AND claimed_at IS NOT NULL AND claimed_at < ?`,
        )
        .all(before) as any[]
    ).map((r) => ({ id: r.id, itemKey: r.item_key, action: r.action, subject: r.subject }));
  }

  /** A stuck `closing` claim becomes pending again (leaving is safe to
   *  repeat). */
  release(id: number): void {
    this.db.prepare(`UPDATE intake_group_requests SET status = 'pending', claimed_at = NULL WHERE id = ? AND status = 'closing'`).run(id);
  }

  group(itemKey: string): IntakeGroupRow | null {
    const r = this.db
      .prepare(`SELECT item_key, jid, status, reason, created_at FROM intake_groups WHERE item_key = ?`)
      .get(itemKey) as any;
    return r ? { itemKey: r.item_key, jid: r.jid, status: r.status, reason: r.reason, createdAt: r.created_at } : null;
  }

  /** The item whose active group has this JID. */
  itemForGroup(jid: string): string | null {
    const r = this.db
      .prepare(`SELECT item_key FROM intake_groups WHERE jid = ? AND status = 'active'`)
      .get(jid) as { item_key: string } | undefined;
    return r?.item_key ?? null;
  }

  activeGroups(): Array<{ itemKey: string; jid: string }> {
    return (
      this.db.prepare(`SELECT item_key, jid FROM intake_groups WHERE status = 'active' AND jid IS NOT NULL`).all() as any[]
    ).map((r) => ({ itemKey: r.item_key, jid: r.jid }));
  }

  /** Creation times of every group the bot created, or may have created
   *  (outcome unknown): the bot's own budget. */
  createdTimes(): string[] {
    return (
      this.db
        .prepare(`SELECT created_at FROM intake_groups WHERE jid IS NOT NULL OR status = 'unknown'`)
        .all() as Array<{ created_at: string }>
    ).map((r) => r.created_at);
  }

  recordGroup(
    itemKey: string,
    row: { jid: string | null; subject: string | null; status: string; reason: string | null; members?: string[] },
  ): void {
    this.db
      .prepare(
        `INSERT INTO intake_groups (item_key, jid, subject, status, reason, created_at, members_json)
         VALUES (?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(item_key) DO UPDATE SET jid = COALESCE(excluded.jid, jid), subject = COALESCE(excluded.subject, subject),
           status = excluded.status, reason = excluded.reason, members_json = COALESCE(excluded.members_json, members_json)`,
      )
      .run(
        itemKey,
        row.jid,
        row.subject,
        row.status,
        row.reason,
        new Date().toISOString(),
        row.members ? JSON.stringify([...row.members].sort()) : null,
      );
  }

  markClosed(itemKey: string, reason: string): void {
    this.db
      .prepare(`UPDATE intake_groups SET status = 'closed', reason = ?, closed_at = ? WHERE item_key = ?`)
      .run(reason, new Date().toISOString(), itemKey);
  }

  /** Store an operator message for an item. A message stored before (the
   *  same WhatsApp id in the same chat) is ignored. Returns true when new. */
  recordInbound(input: { itemKey: string; chatId: string; waMsgId: string; text: string; inputKind: InputKind }): boolean {
    const res = this.db
      .prepare(
        `INSERT OR IGNORE INTO intake_inbound (item_key, chat_id, wa_msg_id, text, received_at, input_kind)
         VALUES (?, ?, ?, ?, ?, ?)`,
      )
      .run(input.itemKey, input.chatId, input.waMsgId, input.text, new Date().toISOString(), input.inputKind);
    return Number(res.changes) > 0;
  }

  /** The item a message the bot sent belongs to (its outbound row's source
   *  is `intake:<n>`), from the WhatsApp message id a reply quotes. */
  itemForSentMessage(msgId: string): string | null {
    const r = this.db.prepare(`SELECT source FROM outbound_queue WHERE msg_id = ?`).get(msgId) as { source: string } | undefined;
    const m = r ? /^intake:(\d+)$/.exec(r.source) : null;
    return m ? m[1] : null;
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

  /** Outbound rows to `jid` not yet sent (a group is left only after its
   *  last messages went out). */
  unsentTo(jid: string): number {
    const r = this.db
      .prepare(`SELECT COUNT(*) AS n FROM outbound_queue WHERE target = ? AND status IN ('pending', 'in_flight')`)
      .get(jid) as { n: number };
    return Number(r.n);
  }
}

/** What the executor needs from the connection. */
export interface GroupApi {
  /** Create a group with `participants`; returns its JID and members. */
  create(subject: string, participants: string[]): Promise<{ jid: string; members: string[] }>;
  leave(jid: string): Promise<void>;
  /** False when the bot is not a member of `jid` (it left, or was
   *  removed); null when that cannot be told. Used after a failed leave. */
  isMember?(jid: string): Promise<boolean | null>;
}

export interface GroupExecutorDeps {
  store: IntakeStore;
  api: GroupApi;
  config: IntakeWhatsAppConfig;
  /** The operator's JID, the only participant a group is created with. */
  operatorJid: () => string | null;
  /** True when `jid` is the operator (phone or LID form). */
  isOperator: (jid: string) => Promise<boolean>;
  /** The bot's own ids (phone JID and LID). */
  selfIds: () => string[];
  /** Set the group's membership baseline (the tripwire compares every later
   *  member list with it); with a reason, the group starts disabled. */
  seedMembers: (jid: string, members: string[], disabledReason: string | null) => void;
  /** A message to the operator's DM (through the outbound queue). */
  alertOperator: (text: string, dedupKey: string) => void;
  onActive: (jid: string) => void;
  onClosed: (jid: string) => void;
  log: { info: (o: object, m: string) => void; warn: (o: object, m: string) => void };
  nowMs?: () => number;
}

/** A close request waits at most this long for the group's last messages. */
const CLOSE_WAIT_MS = 10 * 60 * 1000;

/** Drains `intake_group_requests`, one request at a time. */
export class GroupExecutor {
  private busy = false;
  constructor(private readonly d: GroupExecutorDeps) {}

  private now(): number {
    return this.d.nowMs?.() ?? Date.now();
  }

  async tick(): Promise<void> {
    if (this.busy) return;
    this.busy = true;
    try {
      this.reconcileClaims();
      for (const req of this.d.store.pendingRequests(10, this.now())) {
        if (req.action === "create") await this.create(req);
        else await this.close(req);
      }
    } finally {
      this.busy = false;
    }
  }

  /** Claims a stopped bot left behind: a creation's outcome is unknown and
   *  is never repeated (it could make a second group); a close is tried
   *  again. */
  private reconcileClaims(): void {
    const { store, log } = this.d;
    for (const c of store.stuckClaims(this.now())) {
      if (c.action === "create") {
        const why = "the bot stopped while creating the group; the group may exist";
        store.recordGroup(c.itemKey, { jid: null, subject: c.subject, status: "unknown", reason: why });
        store.finishRequest(c.id, "failed", why);
        this.d.alertOperator(
          `Item #${c.itemKey}: ${why}. If a group "${c.subject ?? `#${c.itemKey}`}" exists, leave it by hand; the item's thread runs in the DM.`,
          `intake:group-unknown:${c.id}`,
        );
        log.warn({ item: c.itemKey }, "whatsapp: intake group creation outcome unknown");
      } else {
        store.release(c.id);
      }
    }
  }

  private async create(req: GroupRequest): Promise<void> {
    const { store, config, log } = this.d;
    if (!store.claim(req.id, "creating", this.now())) return;
    const existing = store.group(req.itemKey);
    if (existing) {
      store.finishRequest(req.id, "done", `already ${existing.status}`);
      return;
    }
    const fallback = (reason: string) => {
      store.recordGroup(req.itemKey, { jid: null, subject: req.subject, status: "fallback", reason });
      store.finishRequest(req.id, "failed", reason);
      log.warn({ item: req.itemKey, reason }, "whatsapp: intake group not created — thread runs in the DM");
    };
    if (store.closeRequested(req.itemKey)) return fallback("the item was closed before the group was created");
    if (!config.refinementGroups) return fallback("refinement groups are disabled");
    if (!groupBudgetAllows(store.createdTimes(), this.now(), config.maxGroupsPerDay)) {
      return fallback(`the limit of ${config.maxGroupsPerDay} new groups per 24 hours is reached`);
    }
    const operator = this.d.operatorJid();
    if (!operator) return fallback("no operator number (WHATSAPP_ALLOWED_DM_JIDS) to add");
    const subject = (req.subject ?? `#${req.itemKey}`).slice(0, 100);
    let g: { jid: string; members: string[] };
    try {
      g = await this.d.api.create(subject, [operator]);
    } catch (e) {
      return fallback(`creating the group failed: ${(e as Error).message}`);
    }
    // The membership baseline is the create response, not the first list a
    // message shows. A member that is neither the bot nor the operator
    // disables the group (no command from it is read) and alerts the
    // operator.
    const strangers = await unexpectedMembers(g.members, this.d.selfIds(), this.d.isOperator);
    const tripped = strangers.length > 0 ? `unexpected members at creation: ${strangers.length}` : null;
    this.d.seedMembers(g.jid, g.members, tripped);
    store.recordGroup(req.itemKey, { jid: g.jid, subject, status: "active", reason: tripped, members: g.members });
    store.finishRequest(req.id, "done", g.jid);
    this.d.onActive(g.jid);
    log.info({ item: req.itemKey, jid: g.jid, members: g.members.length }, "whatsapp: intake group created");
    if (tripped) {
      this.d.alertOperator(
        `Item #${req.itemKey}: the new WhatsApp group has ${strangers.length} member(s) besides the bot and you. ` +
          `Commands from that group are ignored; use the DM (#${req.itemKey} …) or the dashboard.`,
        `intake:group-tripped:${g.jid}`,
      );
    }
    // The item was closed while the group was being created: leave now.
    if (store.closeRequested(req.itemKey)) {
      for (const close of store.pendingRequests(50, this.now() + 366 * 24 * 60 * 60 * 1000).filter((r) => r.itemKey === req.itemKey && r.action === "close")) {
        await this.close(close, true);
      }
    }
  }

  private async close(req: GroupRequest, immediate = false): Promise<void> {
    const { store, log } = this.d;
    const g = store.group(req.itemKey);
    if (!g || g.status !== "active" || !g.jid) {
      if (store.claim(req.id, "closing", this.now())) store.finishRequest(req.id, "done", "no active group");
      return;
    }
    const waited = this.now() - Date.parse(req.enqueuedAt);
    if (!immediate && store.unsentTo(g.jid) > 0 && waited < CLOSE_WAIT_MS) return; // last messages first
    if (!store.claim(req.id, "closing", this.now())) return;
    let left = false;
    let error = "";
    try {
      await this.d.api.leave(g.jid);
      left = true;
    } catch (e) {
      error = (e as Error).message;
      // Leaving a group the bot is no longer in fails; that is the outcome
      // wanted. Only a confirmed non-membership counts.
      const member = await this.d.api.isMember?.(g.jid).catch(() => null);
      if (member === false) left = true;
    }
    if (left) {
      store.markClosed(req.itemKey, "item closed");
      store.finishRequest(req.id, "done", "left");
      this.d.onClosed(g.jid);
      log.info({ item: req.itemKey, jid: g.jid }, "whatsapp: left intake group");
      return;
    }
    const attempts = req.attempts + 1;
    if (attempts >= MAX_CLOSE_ATTEMPTS) {
      store.finishRequest(req.id, "failed", error);
      this.d.alertOperator(
        `Item #${req.itemKey}: leaving its WhatsApp group failed ${attempts} times (${error}). Leave the group by hand.`,
        `intake:group-close-failed:${req.id}`,
      );
      log.warn({ item: req.itemKey, err: error }, "whatsapp: leaving intake group failed — given up");
      return;
    }
    store.retryLater(req.id, attempts, error, this.now());
    log.warn({ item: req.itemKey, err: error, attempts }, "whatsapp: leaving intake group failed — will retry");
  }
}
