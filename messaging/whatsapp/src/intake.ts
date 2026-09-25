// Issue pipeline surface on WhatsApp (ADR-036).
//
// The pipeline itself runs in Rust (`nucleus intake tick`). The bot does the
// three things only the WhatsApp connection can do:
//
//   1. Create and leave an item's WhatsApp group. The pipeline queues a
//      request in `intake_group_requests` (a queue table: Rust inserts, the
//      bot drains); the bot records the outcome in its own `intake_groups`
//      table, which Rust reads. A created group contains the bot and the
//      operator only.
//   2. Route the operator's messages to an item: every message in an
//      item's group, and DM messages that start with the item marker
//      (`#12 …`) or quote a message the pipeline sent for that item. The
//      bot stores them in `intake_inbound` and runs `nucleus intake tick`;
//      the Rust side reads them, decides approvals, and replies through
//      the outbound queue.
//   3. Keep the intake groups in the target allowlist while they are
//      active, so the outbound drain (target policy + secret filter) can
//      send to them, and remove them when the item closes.
//
// Group creation is rate-limited twice: the pipeline requests at most
// `[intake.whatsapp] max_groups_per_day` groups in 24 hours, and the bot
// refuses to create more than that itself. Automated group creation from a
// personal account can trigger WhatsApp's anti-spam checks; a refused
// request is recorded as `fallback` and the item's thread runs in the DM.

import { DatabaseSync } from "node:sqlite";

/** A group request the pipeline queued. */
export interface GroupRequest {
  id: number;
  itemKey: string;
  action: "create" | "close";
  subject: string | null;
  enqueuedAt: string;
}

export interface IntakeGroupRow {
  itemKey: string;
  jid: string | null;
  status: "active" | "fallback" | "closed";
  reason: string | null;
  createdAt: string;
}

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
        dedup_key   TEXT
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
        closed_at  TEXT
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
        received_at TEXT NOT NULL
      );
      CREATE UNIQUE INDEX IF NOT EXISTS idx_intake_inbound_msg
        ON intake_inbound(chat_id, wa_msg_id);
    `);
  }

  pendingRequests(limit = 10): GroupRequest[] {
    return (
      this.db
        .prepare(
          `SELECT id, item_key, action, subject, enqueued_at FROM intake_group_requests
            WHERE status = 'pending' ORDER BY id LIMIT ?`,
        )
        .all(limit) as any[]
    ).map((r) => ({ id: r.id, itemKey: r.item_key, action: r.action, subject: r.subject, enqueuedAt: r.enqueued_at }));
  }

  finishRequest(id: number, status: "done" | "failed", result: string): void {
    this.db
      .prepare(`UPDATE intake_group_requests SET status = ?, result = ?, handled_at = ? WHERE id = ?`)
      .run(status, result, new Date().toISOString(), id);
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

  /** Creation times of every group the bot created (the bot's own budget). */
  createdTimes(): string[] {
    return (
      this.db.prepare(`SELECT created_at FROM intake_groups WHERE jid IS NOT NULL`).all() as Array<{ created_at: string }>
    ).map((r) => r.created_at);
  }

  recordGroup(itemKey: string, row: { jid: string | null; subject: string | null; status: string; reason: string | null }): void {
    this.db
      .prepare(
        `INSERT INTO intake_groups (item_key, jid, subject, status, reason, created_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(item_key) DO UPDATE SET jid = COALESCE(excluded.jid, jid), subject = COALESCE(excluded.subject, subject),
           status = excluded.status, reason = excluded.reason`,
      )
      .run(itemKey, row.jid, row.subject, row.status, row.reason, new Date().toISOString());
  }

  markClosed(itemKey: string, reason: string): void {
    this.db
      .prepare(`UPDATE intake_groups SET status = 'closed', reason = ?, closed_at = ? WHERE item_key = ?`)
      .run(reason, new Date().toISOString(), itemKey);
  }

  /** Store an operator message for an item. A message stored before (the
   *  same WhatsApp id in the same chat) is ignored. Returns true when new. */
  recordInbound(input: { itemKey: string; chatId: string; waMsgId: string; text: string }): boolean {
    const res = this.db
      .prepare(
        `INSERT OR IGNORE INTO intake_inbound (item_key, chat_id, wa_msg_id, text, received_at)
         VALUES (?, ?, ?, ?, ?)`,
      )
      .run(input.itemKey, input.chatId, input.waMsgId, input.text, new Date().toISOString());
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
}

export interface GroupExecutorDeps {
  store: IntakeStore;
  api: GroupApi;
  config: IntakeWhatsAppConfig;
  /** The operator's JID, the only participant a group is created with. */
  operatorJid: () => string | null;
  onActive: (jid: string) => void;
  onClosed: (jid: string) => void;
  log: { info: (o: object, m: string) => void; warn: (o: object, m: string) => void };
  nowMs?: () => number;
}

/** A close request waits at most this long for the group's last messages. */
const CLOSE_WAIT_MS = 10 * 60 * 1000;

/** Drains `intake_group_requests`. One request at a time; a failure is
 *  recorded and never retried automatically (a retried creation could make
 *  a second group). */
export class GroupExecutor {
  private busy = false;
  constructor(private readonly d: GroupExecutorDeps) {}

  async tick(): Promise<void> {
    if (this.busy) return;
    this.busy = true;
    try {
      for (const req of this.d.store.pendingRequests()) {
        if (req.action === "create") await this.create(req);
        else await this.close(req);
      }
    } finally {
      this.busy = false;
    }
  }

  private async create(req: GroupRequest): Promise<void> {
    const { store, config, log } = this.d;
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
    if (!config.refinementGroups) return fallback("refinement groups are disabled");
    const now = this.d.nowMs?.() ?? Date.now();
    if (!groupBudgetAllows(store.createdTimes(), now, config.maxGroupsPerDay)) {
      return fallback(`the limit of ${config.maxGroupsPerDay} new groups per 24 hours is reached`);
    }
    const operator = this.d.operatorJid();
    if (!operator) return fallback("no operator number (WHATSAPP_ALLOWED_DM_JIDS) to add");
    const subject = (req.subject ?? `#${req.itemKey}`).slice(0, 100);
    try {
      const g = await this.d.api.create(subject, [operator]);
      store.recordGroup(req.itemKey, { jid: g.jid, subject, status: "active", reason: null });
      store.finishRequest(req.id, "done", g.jid);
      this.d.onActive(g.jid);
      log.info({ item: req.itemKey, jid: g.jid, members: g.members.length }, "whatsapp: intake group created");
    } catch (e) {
      fallback(`creating the group failed: ${(e as Error).message}`);
    }
  }

  private async close(req: GroupRequest): Promise<void> {
    const { store, log } = this.d;
    const g = store.group(req.itemKey);
    if (!g || g.status !== "active" || !g.jid) {
      store.finishRequest(req.id, "done", "no active group");
      return;
    }
    const waited = (this.d.nowMs?.() ?? Date.now()) - Date.parse(req.enqueuedAt);
    if (store.unsentTo(g.jid) > 0 && waited < CLOSE_WAIT_MS) return; // last messages first
    try {
      await this.d.api.leave(g.jid);
      store.markClosed(req.itemKey, "item closed");
      store.finishRequest(req.id, "done", "left");
      this.d.onClosed(g.jid);
      log.info({ item: req.itemKey, jid: g.jid }, "whatsapp: left intake group");
    } catch (e) {
      store.finishRequest(req.id, "failed", (e as Error).message);
      log.warn({ item: req.itemKey, err: (e as Error).message }, "whatsapp: leaving intake group failed");
    }
  }
}
