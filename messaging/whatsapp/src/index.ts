import {
  default as makeWASocket,
  useMultiFileAuthState,
  fetchLatestWaWebVersion,
  makeCacheableSignalKeyStore,
  downloadMediaMessage,
  Browsers,
  DisconnectReason,
  type WAMessage,
  type WASocket,
} from "@whiskeysockets/baileys";
import { Boom } from "@hapi/boom";
import pino from "pino";
import qrcodeTerminal from "qrcode-terminal";
import * as qrcodeImg from "qrcode";
import { spawn } from "node:child_process";
import path from "node:path";
import fs from "node:fs";

import { loadConfig, normalizeSenderId, type Config } from "./config.js";
import { SessionPool, sleepUntilNext4am } from "./claude_session.js";
import {
  ChatSessionStore,
  OutboundQueueStore,
  PendingPlansStore,
  shortPlanId,
  type OutboundRow,
} from "./db.js";
import { record as recordDiary } from "./diary.js";
import { alertDiscordHome } from "./discord_alert.js";
import { formatReply as sharedFormatReply } from "./format.js";
import {
  buildOutboundContent,
  cleanupMedia,
  DRAIN_WATCHDOG_MS,
  MAX_MEDIA_SENDS_PER_TICK,
  sendTimeoutFor,
  sweepOutboundStaging,
} from "./outbound.js";
import { classifyCaption } from "./caption.js";
import { fireEnrichJob, runImportJob } from "./doc_jobs.js";
import { JobStore, JOBS_TMUX_SESSION, startJob, withQuickWindow } from "./jobs.js";
import { DocStore } from "./docstore.js";
import { makeVaultManifestHook } from "./docstore_vault.js";
import { transcribe } from "./transcribe.js";
import {
  planCapture,
  applyPlan,
  interpretResponse,
  formatRundown,
  BRAINDUMP_TMUX_SESSION,
  type AppliedOp,
  type PlanForReview,
} from "./braindump.js";

// Every tmux session this process spawns claude windows into. Defined once
// and reused for both pool construction and the boot-time orphan wipe, so
// adding a pool can't forget the wipe again — nucleus-whatsapp-dm was
// missing from a hand-maintained wipe list for 3 weeks (ADR-005b landed
// without it), and the orphan it left turned into the 2026-06-11 DM outage.
const GROUP_TMUX_SESSION = "nucleus-whatsapp";
const DM_TMUX_SESSION = "nucleus-whatsapp-dm";
// ADR-013: the jobs session is IN the wipe list on purpose — a restart
// kills in-flight job windows, which is what makes "orphaned" mean dead
// rather than maybe-still-running.
const ALL_TMUX_SESSIONS = [GROUP_TMUX_SESSION, DM_TMUX_SESSION, BRAINDUMP_TMUX_SESSION, JOBS_TMUX_SESSION];

// Synchronous destination — no worker-thread buffering. Logs appear in stdout
// as soon as they're emitted, which matters when tailing to debug what stage
// a message is at.
const log = pino(
  { level: process.env.NUCLEUS_LOG ?? "info" },
  pino.destination({ sync: true }),
);

const baileysLogger = pino({ level: "silent" });

/**
 * Resolved allowlist — JID → role. The role decides which pipeline runs:
 *
 *   "whatsapp-group" — conversational. Send a message, get a reply via
 *                      SessionPool. Voice memos are transcribed → asked
 *                      → reply.
 *   "braindump"      — capture-only. Inbound messages get classified and
 *                      filed into the PARA-organized vault (T3). Voice
 *                      memos are transcribed → filed as PARA notes. The
 *                      bot may reply with confirmation or escalate when
 *                      classification is uncertain (Phase 4).
 *
 * Built at connection.open from the four allowlist sources in Config
 * (group chatIds + groupNames, braindump chatIds + groupNames). Empty
 * until populated. Held in module scope so the message handler can read it
 * without plumbing through args.
 */
type ChatRole = "whatsapp-group" | "braindump" | "dm";
const allowedJids = new Map<string, ChatRole>();

/** JID-shape discriminator (ADR-005b). Groups end `@g.us`; DMs end
 *  `@s.whatsapp.net` or `@lid` (modern WhatsApp surfaces some DMs
 *  under LIDs). Anything else (channels, broadcasts) is unsupported. */
function chatType(jid: string): "group" | "dm" {
  return jid.endsWith("@g.us") ? "group" : "dm";
}

/** Resolve the role for an inbound chatId. Groups use literal-JID
 *  lookup; DMs normalize the chatId user-part to digits and check
 *  against the DM-sender set, matching either @s.whatsapp.net or
 *  @lid presentations of the same operator. */
function resolveRole(chatId: string, config: Config): ChatRole | undefined {
  const direct = allowedJids.get(chatId);
  if (direct) return direct;
  if (chatType(chatId) === "dm") {
    const digits = normalizeSenderId(chatId);
    if (digits && config.allowedDmSenders.has(digits)) return "dm";
  }
  return undefined;
}

/** Reverse lookup populated by resolveAllowlist: group name (case-
 *  sensitive, as configured in .env) → JID. Used by the outbound queue
 *  drainer to translate "Alfred" / "Brain Dump" targets into JIDs. */
const groupNameToJid = new Map<string, string>();

// Tightened from 5s to 1s so braindump-ack messages (queued by the
// planning Claude session via src/ack.ts) land within ~1s — close to
// instant for the operator.
const OUTBOUND_DRAIN_INTERVAL_MS = 1_000;
const OUTBOUND_MAX_ATTEMPTS = 5;

// Connection-rot watchdog. Baileys can land in a state where the socket
// is "connected" to its event handlers but every sendMessage throws
// "Connection Closed" — inbound traffic still flows, but outbound is
// silently broken. We've seen the bot sit in this state for hours,
// accumulating failed outbound_queue rows with no alert. Solution: count
// consecutive "Connection Closed"-shaped failures across the whole drain
// loop, alert + exit(1) (launchd respawns with fresh Baileys state) once
// the threshold is hit, reset on the first successful send.
const CONNECTION_ROT_THRESHOLD = 5;
let consecutiveConnectionFailures = 0;

// Reconnects fire the connection.update("open") branch every time, which
// re-invokes startOutboundDrain / startPlanExpirySweep. Without storing
// the handles and clearing on re-entry, every reconnect leaked another
// parallel setInterval — N reconnects → N concurrent drains racing on
// the same row, multiplying a single transient send failure by N and
// tripping the rot watchdog in milliseconds. Incident 2026-05-22.
let outboundDrainTimer: NodeJS.Timeout | null = null;
let planExpirySweepTimer: NodeJS.Timeout | null = null;

// Re-entrancy guard for the outbound drain. setInterval fires the async
// callback every OUTBOUND_DRAIN_INTERVAL_MS regardless of whether the prior
// tick has resolved. A single sock.sendMessage takes ~2s over the network,
// which exceeds the 1s interval — so without this guard tick N+1 starts
// while tick N is still awaiting sendMessage, re-SELECTs the same still-
// pending row (pending() doesn't claim/lock; markSent only runs post-send),
// and sends it a second time. Result: one outbound_queue row, two WhatsApp
// messages. Hit DSU forwards specifically because long bodies send slower.
// Incident 2026-05-28 (duplicate DSU notifications). The 2026-05-22
// clearInterval guard stopped reconnect-spawned parallel drains but not a
// single timer overlapping itself.
let outboundDraining = false;

// Hang protection (ADR-020). The re-entrancy flag above is correct but
// fail-closed: if sock.sendMessage HANGS (never settles — distinct from
// rejecting), `outboundDraining` stays true forever and the queue silently
// stops draining. Two layers:
//  1. Per-send timeout — a send that hasn't settled in its kind's timeout
//     (text 20s, media 90s — ADR-018) is treated as failed (retried,
//     bounded by OUTBOUND_MAX_ATTEMPTS) and counts toward connection-rot;
//     the tick aborts (a hung socket won't recover row-to-row). The
//     original promise is kept: if it settles late with success we
//     markSent, which suppresses the next tick's retry — closing most of
//     the duplicate window retry-on-timeout opens.
//  2. Drain watchdog — if outboundDraining has been true longer than
//     DRAIN_WATCHDOG_MS (every await in the tick is timeout-bounded, so
//     this means the boundedness assumption itself broke), alert + exit(1)
//     for a launchd respawn. Force-resetting the flag instead would revive
//     the 2026-05-28 overlapping-tick double-send if the zombie tick is
//     still alive; exit matches the connection-rot precedent.
// Constants + content building live in outbound.ts (DRAIN_WATCHDOG_MS is
// DERIVED from the per-kind timeouts there, so the bound can't drift).
let drainTickStartedAt: number | null = null;

class SendTimeoutError extends Error {
  constructor(ms: number) {
    super(`sendMessage timed out after ${ms}ms (may still deliver late)`);
    this.name = "SendTimeoutError";
  }
}

function withTimeout<T>(p: Promise<T>, ms: number): Promise<T> {
  let timer: NodeJS.Timeout;
  return Promise.race([
    p,
    new Promise<never>((_, reject) => {
      timer = setTimeout(() => reject(new SendTimeoutError(ms)), ms);
    }),
  ]).finally(() => clearTimeout(timer!));
}

// ADR-005a: braindump plan timeout + sweep cadence.
const PLAN_TIMEOUT_MS = 30 * 60 * 1000;
const PLAN_SWEEP_INTERVAL_MS = 5 * 60 * 1000;

// ── ADR-013: act-on-media job settings ─────────────────────────────────────

/** Quick window before an act job promotes to deferred delivery. A cold
 *  claude spawn alone is 5-20s, so most non-trivial asks WILL promote —
 *  that's fine (ack at ~30s, answer follows). Raise to 60s if the
 *  two-message dance grates; don't shrink below 30s. */
const ACT_QUICK_WINDOW_MS = 30_000;

/** Code-owned job persona — never the operator's DM persona; the persona
 *  signature comes from formatReply at send time. */
const JOB_ACT_SYSTEM_PROMPT = `You are answering exactly one instruction
about one attached document on behalf of the operator. Read the file, do
what was asked. Your final message IS the WhatsApp reply — answer directly,
no preamble, no narration, match the instruction's language. You may search
the document library for cross-references via the docs CLI (Bash); never
deliver files, never use other tools.`;

// ── ADR-018: document library wiring ──────────────────────────────────────

/** Bash patterns the DM pool pre-approves so the session can look up and
 *  deliver documents without classifier prompting (ack.ts precedent). */
const DOC_TOOL_ALLOWLIST = [
  "Bash(npx --prefix messaging/whatsapp tsx messaging/whatsapp/src/docs.ts:*)",
  "Bash(npx --prefix messaging/whatsapp tsx messaging/whatsapp/src/enqueue-media.ts:*)",
];

/** Code-owned capability blurb appended to the DM persona — the persona
 *  file is operator-owned, so the mechanics live here, not there. */
const DOCS_CAPABILITY_PROMPT = `## Document library (ADR-018)

You can retrieve and manage the operator's local document library. All
commands run from the workspace root via Bash and print JSON lines:

- find:    npx --prefix messaging/whatsapp tsx messaging/whatsapp/src/docs.ts find <query…>
- list:    npx --prefix messaging/whatsapp tsx messaging/whatsapp/src/docs.ts list [--tag t]
- rename:  npx --prefix messaging/whatsapp tsx messaging/whatsapp/src/docs.ts rename <id> --name "…"
- deliver: npx --prefix messaging/whatsapp tsx messaging/whatsapp/src/enqueue-media.ts --doc <id> [--caption "…"]

Rules: deliveries go ONLY to the operator's own DM — enqueue-media's --doc
mode has no target flag and refuses one; never try to send a document to a
group or anyone else. Handle documents BY REFERENCE: use ids and metadata,
never Read a library file unless the operator explicitly asks you to act
on its contents. Files over 64MB can't be delivered.`;

async function main() {
  const discover = process.argv.includes("--discover");
  const workspaceRoot =
    process.env.NUCLEUS_WORKSPACE_ROOT ??
    path.resolve(import.meta.dirname, "..", "..", "..");
  const config = loadConfig(workspaceRoot, discover);
  configurePersona(config.personaDisplayName);

  log.info(
    {
      workspaceRoot,
      claudeBin: config.claudeBin,
      allowedChats: config.allowedChatIds,
      allowedGroups: config.allowedGroupNames,
      brainDumpChats: config.brainDumpChatIds,
      brainDumpGroups: config.brainDumpGroupNames,
      vaultPath: config.vaultPath,
      discover: config.discoverMode,
    },
    "whatsapp: starting",
  );

  const anyAlfred = config.allowedChatIds.length || config.allowedGroupNames.length;
  const anyBrainDump = config.brainDumpChatIds.length || config.brainDumpGroupNames.length;
  if (!anyAlfred && !anyBrainDump && !config.discoverMode) {
    log.warn(
      "whatsapp: no WHATSAPP_ALLOWED_* or WHATSAPP_BRAINDUMP_* groups/chats configured — bot will respond to nothing. Set at least one or run --discover.",
    );
  }

  const store = new ChatSessionStore(config.dbPath);
  // Note: pending_classifications schema still lives in ChatSessionStore's
  // CREATE block (kept for forward-compat); the multi-op braindump pipeline
  // doesn't use it — corrections happen via follow-up captures + move ops
  // (see CLAUDE.md Rule 9 + ADR-005).
  const outbound = new OutboundQueueStore(config.dbPath);
  // ADR-018: collect staged media files orphaned by a crash between
  // markSent and unlink, terminal rows whose unlink failed, or an
  // enqueue-media crash between copy and INSERT.
  sweepOutboundStaging(config.outboundStagingDir, outbound.pendingMediaPaths());
  // ADR-005a: holds brain-dump plans pending operator review.
  const plansStore = new PendingPlansStore(config.dbPath);
  // ADR-018: the document library (inbound media archives here; the DM
  // session retrieves via the docs/enqueue-media CLIs).
  const docStore = new DocStore({
    dbPath: config.documentsDbPath,
    documentsDir: config.documentsDir,
    onManifestChange: makeVaultManifestHook(config),
  });
  // ADR-013: jobs ledger. Sweep rows orphaned by a restart BEFORE the
  // drain starts so the interruption notes are first in the queue. Only
  // kinds that promised the operator a reply get a note; enrich is a
  // silent feature and stays silent in failure.
  const jobStore = new JobStore({ dbPath: config.jobsDbPath });
  for (const orphan of jobStore.sweepOrphans()) {
    log.warn({ jobId: orphan.id, kind: orphan.kind }, "whatsapp: job orphaned by restart");
    if (orphan.kind === "act" || orphan.kind === "vault-import") {
      const digits = normalizeSenderId(orphan.chatId);
      if (digits) {
        outbound.enqueue({
          target: digits,
          source: "job-orphan",
          body: formatReply(
            `⚠️ tarefa interrompida pelo restart: ${orphan.instruction.slice(0, 80)} (job ${orphan.id.slice(0, 8)}) — reenvie se ainda quiser`,
          ),
        });
      }
    }
  }

  // Tear down any leftover tmux sessions from a previous run before we own
  // fresh windows — startup is the safe time to clean orphans from prior
  // crashes. The pools are in-memory, so any surviving window is an orphan
  // by definition. ALL_TMUX_SESSIONS is derived from the same constants the
  // pools are built with; never hand-list session names here.
  for (const sessionName of ALL_TMUX_SESSIONS) {
    await new Promise<void>((resolve) => {
      const child = spawn("tmux", ["kill-session", "-t", sessionName], {
        stdio: "ignore",
      });
      child.on("close", () => resolve());
      child.on("error", () => resolve());
    });
  }

  const sessions = new SessionPool({
    workspaceRoot: config.workspaceRoot,
    appendSystemPrompt: config.appendSystemPromptGroup,
    permissionMode: config.permissionMode,
    disallowedTools: config.disallowedTools,
    tmuxSession: GROUP_TMUX_SESSION,
    idleTimeoutMs: 4 * 60 * 60 * 1000, // 4h
    agentLabel: "whatsapp",
    reviewNudgeInterval: config.skillNudgeInterval,
  });

  // ADR-005b: a second pool with the DM-context persona. Group JIDs end
  // `@g.us` and DM JIDs end `@s.whatsapp.net`, so the keys never collide
  // — we use chatId as the key in both pools, and the dispatch chooses
  // which pool to talk to based on chatType.
  // ADR-018: the DM pool gets the document-library capability — the two
  // CLIs pre-approved past the auto-mode classifier, and a code-owned
  // capability blurb appended to the (operator-owned) persona.
  const sessionsDm = new SessionPool({
    workspaceRoot: config.workspaceRoot,
    appendSystemPrompt: `${config.appendSystemPromptDm}\n\n${DOCS_CAPABILITY_PROMPT}`,
    permissionMode: config.permissionMode,
    disallowedTools: config.disallowedTools,
    allowedTools: DOC_TOOL_ALLOWLIST,
    tmuxSession: DM_TMUX_SESSION,
    idleTimeoutMs: 4 * 60 * 60 * 1000, // 4h
    agentLabel: "whatsapp",
    reviewNudgeInterval: config.skillNudgeInterval,
  });

  // Background idle reaper covers both pools.
  setInterval(async () => {
    try {
      const n = (await sessions.reapIdle()) + (await sessionsDm.reapIdle());
      if (n > 0) log.info({ reaped: n }, "whatsapp: reaped idle sessions");
    } catch (e) {
      log.warn({ err: (e as Error).message }, "whatsapp: reap failed");
    }
  }, 30 * 60 * 1000);

  // Background daily 04:00 rotation. Summarizes each active chat into the
  // whatsapp daily diary, spawns a fresh primed session, and persists the
  // new session-id to chat_sessions so any restart picks up the rotated id.
  // Runs the same routine on both pools (group + DM).
  (async () => {
    while (true) {
      await sleepUntilNext4am();
      const dbUpdate = async (chatId: string, newSessionId: string) => {
        store.save(chatId, newSessionId, true);
      };
      try {
        const groupStats = await sessions.dailyRotate(config.diaryRoot, dbUpdate);
        const dmStats = await sessionsDm.dailyRotate(config.diaryRoot, dbUpdate);
        log.info(
          { group: groupStats, dm: dmStats },
          "whatsapp: daily rotation done",
        );
      } catch (e) {
        log.error(
          { err: (e as Error).message },
          "whatsapp: daily rotation crashed",
        );
      }
    }
  })();

  await connect(config, store, sessions, sessionsDm, outbound, plansStore, docStore, jobStore);
}

async function connect(
  config: Config,
  store: ChatSessionStore,
  sessions: SessionPool,
  sessionsDm: SessionPool,
  outbound: OutboundQueueStore,
  plansStore: PendingPlansStore,
  docStore: DocStore,
  jobStore: JobStore,
): Promise<void> {
  const authDir = path.join(config.workspaceRoot, "messaging/whatsapp/auth");
  fs.mkdirSync(authDir, { recursive: true });
  const { state, saveCreds } = await useMultiFileAuthState(authDir);

  // Pin to WhatsApp Web's currently-published protocol version. Critical: a
  // stale Baileys default causes silent pairing failure (status 405 loop).
  const { version, isLatest } = await fetchLatestWaWebVersion({});
  log.info({ version, isLatest }, "whatsapp: protocol version");

  const sock = makeWASocket({
    version,
    auth: {
      creds: state.creds,
      keys: makeCacheableSignalKeyStore(state.keys, baileysLogger),
    },
    browser: Browsers.macOS("Chrome"),
    markOnlineOnConnect: false,
    syncFullHistory: false,
    logger: baileysLogger as any,
  });

  sock.ev.on("creds.update", saveCreds);

  sock.ev.on("connection.update", (update) => {
    const { connection, lastDisconnect, qr } = update;
    if (qr) {
      log.info("whatsapp: pair with your phone — Linked Devices → Link a Device");
      // Render to terminal as ASCII (small enough to fit) AND save as PNG, then
      // open the PNG in the system image viewer for a clean scan target.
      qrcodeTerminal.generate(qr, { small: true });
      const qrPath = path.join(config.workspaceRoot, "messaging/whatsapp/auth/qr.png");
      qrcodeImg
        .toFile(qrPath, qr, { width: 512, margin: 4, errorCorrectionLevel: "M" })
        .then(() => {
          log.info({ qrPath }, "whatsapp: QR saved as PNG");
          // Open in macOS Preview (or default image viewer). Non-fatal on failure.
          spawn("open", [qrPath], { detached: true, stdio: "ignore" }).unref();
        })
        .catch((e) => log.warn({ err: e?.message }, "whatsapp: PNG QR write failed"));
    }
    if (connection === "open") {
      log.info({ user: sock.user?.id }, "whatsapp: connected");
      // Resolve the allowlist asynchronously so handler is ready before any
      // unexpected event fires. Then start the outbound drain — the
      // drainer needs the allowlist to authorize each target.
      resolveAllowlist(sock, config)
        .then(() => {
          startOutboundDrain(sock, outbound, config);
          startPlanExpirySweep(sock, plansStore);
        })
        .catch((e) =>
          log.error({ err: e?.message }, "whatsapp: allowlist resolve failed"),
        );
      recordDiary(config.diaryRoot, "boot", `Connected as ${sock.user?.id ?? "unknown"}`, "OBSERVATION");
    } else if (connection === "close") {
      const reason = (lastDisconnect?.error as Boom)?.output?.statusCode;
      const shouldReconnect = reason !== DisconnectReason.loggedOut;
      log.warn({ reason, shouldReconnect }, "whatsapp: connection closed");
      if (shouldReconnect) {
        const delay = reason === 405 ? 5000 : 2000;
        setTimeout(() => connect(config, store, sessions, sessionsDm, outbound, plansStore, docStore, jobStore).catch((e) => log.error(e, "reconnect failed")), delay);
      } else {
        log.error("whatsapp: logged out — delete auth/ and re-pair");
      }
    }
  });

  sock.ev.on("messages.upsert", async ({ messages, type }) => {
    if (type !== "notify") return;
    for (const msg of messages) {
      await handleMessage(sock, msg, config, store, sessions, sessionsDm, plansStore, docStore, jobStore, outbound).catch((e) => {
        log.error({ err: e?.message }, "whatsapp: handler failed");
      });
    }
  });
}

async function resolveAllowlist(sock: WASocket, config: Config): Promise<void> {
  allowedJids.clear();
  groupNameToJid.clear();

  // Seed with literal JIDs from env. Brain-dump assignments take precedence
  // if a JID somehow appears in both lists (we never want a brain-dump
  // capture chat to also get conversational replies — pick one role).
  for (const jid of config.allowedChatIds) allowedJids.set(jid, "whatsapp-group");
  for (const jid of config.brainDumpChatIds) allowedJids.set(jid, "braindump");
  const wantGroup = new Set(config.allowedGroupNames.map((n) => n.toLowerCase()));
  const wantBrainDump = new Set(config.brainDumpGroupNames.map((n) => n.toLowerCase()));
  if (!wantGroup.size && !wantBrainDump.size) {
    log.info({ allowedJids: [...allowedJids] }, "whatsapp: allowlist resolved (no group lookups needed)");
    return;
  }

  try {
    const groups = await sock.groupFetchAllParticipating();
    const matches: Array<{ jid: string; name: string; role: ChatRole }> = [];
    for (const [jid, meta] of Object.entries(groups)) {
      const name = (meta?.subject ?? "").trim();
      if (!name) continue;
      const lower = name.toLowerCase();
      if (wantBrainDump.has(lower)) {
        allowedJids.set(jid, "braindump");
        groupNameToJid.set(name, jid);
        matches.push({ jid, name, role: "braindump" });
      } else if (wantGroup.has(lower)) {
        allowedJids.set(jid, "whatsapp-group");
        groupNameToJid.set(name, jid);
        matches.push({ jid, name, role: "whatsapp-group" });
      }
    }
    log.info(
      {
        requestedGroup: config.allowedGroupNames,
        requestedBrainDump: config.brainDumpGroupNames,
        matched: matches,
        allowedJids: Object.fromEntries(allowedJids),
      },
      "whatsapp: allowlist resolved",
    );
    const totalRequested = wantGroup.size + wantBrainDump.size;
    if (matches.length < totalRequested) {
      log.warn(
        "whatsapp: one or more group names did not match any participating group — bot will be deaf to them",
      );
    }
  } catch (e) {
    log.error({ err: (e as Error).message }, "whatsapp: groupFetchAllParticipating failed");
  }
}

/** Background drain of the outbound_queue table. Runs every 5s once
 *  the allowlist is resolved. For each pending row:
 *    1. Resolve `target` to a JID via groupNameToJid (or treat as a
 *       literal JID if it already looks like one).
 *    2. Verify the JID is on the allowlist — refuse to send otherwise.
 *    3. sock.sendMessage. On success, markSent. On failure, markFailure
 *       (which keeps it pending until OUTBOUND_MAX_ATTEMPTS).
 *
 *  Bounded batch size per tick to avoid hogging the event loop if a
 *  large backlog accumulates (it won't in normal use, but defense in depth). */
function startOutboundDrain(sock: WASocket, outbound: OutboundQueueStore, config: Config): void {
  if (outboundDrainTimer) clearInterval(outboundDrainTimer);
  outboundDrainTimer = setInterval(async () => {
    // Watchdog: a tick stuck past the bound means an await escaped the
    // per-send timeouts — unknown hang, restart for a clean slate (see the
    // hang-protection comment above).
    if (
      outboundDraining &&
      drainTickStartedAt !== null &&
      Date.now() - drainTickStartedAt > DRAIN_WATCHDOG_MS
    ) {
      const msg = `⚠️ WhatsApp outbound drain stuck >${Math.round(DRAIN_WATCHDOG_MS / 1000)}s — exiting for launchd respawn.`;
      log.error({ stuckSince: new Date(drainTickStartedAt).toISOString() }, msg);
      await withTimeout(alertDiscordHome(msg), 5_000).catch(() => {});
      process.exit(1);
    }
    // Skip this tick if the previous one is still draining — otherwise two
    // ticks grab the same not-yet-markSent row and double-send it. See the
    // outboundDraining comment above.
    if (outboundDraining) return;
    outboundDraining = true;
    drainTickStartedAt = Date.now();
    try {
      let rows: OutboundRow[];
      try {
        rows = outbound.pending(20);
      } catch (e) {
        log.warn({ err: (e as Error).message }, "whatsapp: outbound pending() failed");
        return;
      }
      if (rows.length === 0) return;
      log.info({ count: rows.length }, "whatsapp: draining outbound queue");
      // ADR-018: bound the media budget per tick so a batch of uploads
      // can't monopolize the drain. Skipped media rows stay pending for
      // the next tick (mildly breaks global FIFO; text ordering holds).
      let mediaSentThisTick = 0;
      for (const r of rows) {
        if (r.kind !== "text" && mediaSentThisTick >= MAX_MEDIA_SENDS_PER_TICK) {
          continue;
        }
        const jid = resolveOutboundTarget(r.target, config);
        if (!jid) {
          const { status } = outbound.markFailure(
            r.id,
            `unknown target: ${r.target}`,
            OUTBOUND_MAX_ATTEMPTS,
          );
          if (status === "failed") cleanupMedia(r);
          log.warn({ id: r.id, target: r.target }, "whatsapp: outbound target not in allowlist — failed");
          continue;
        }
        // Build content first: a media row whose file is missing or
        // oversized can NEVER succeed — terminal-fail it instead of
        // burning 5 retries.
        const content = buildOutboundContent(r, config.mediaMaxBytes);
        if ("error" in content) {
          outbound.markFailedTerminal(r.id, content.error);
          cleanupMedia(r);
          log.warn({ id: r.id, err: content.error }, "whatsapp: outbound media row terminal-failed");
          continue;
        }
        try {
          if (process.env.NUCLEUS_WHATSAPP_FORCE_SEND_FAIL === "1") {
            throw new Error("Connection Closed (synthetic — NUCLEUS_WHATSAPP_FORCE_SEND_FAIL)");
          }
          // FORCE_SEND_HANG: never-settling promise so the timeout +
          // watchdog paths are manually testable like the fail path is.
          const sendPromise: Promise<WAMessage | undefined> =
            process.env.NUCLEUS_WHATSAPP_FORCE_SEND_HANG === "1"
              ? new Promise<never>(() => {})
              : sock.sendMessage(jid, content);
          try {
            const sent = await withTimeout(sendPromise, sendTimeoutFor(r.kind));
            outbound.markSent(r.id, sent?.key?.id ?? "");
            cleanupMedia(r);
            if (r.kind !== "text") mediaSentThisTick += 1;
            consecutiveConnectionFailures = 0;
            log.info({ id: r.id, kind: r.kind, target: r.target, jid }, "whatsapp: outbound sent");
          } catch (e) {
            if (!(e instanceof SendTimeoutError)) throw e;
            // Timeout: mark failed (retried next tick, bounded attempts) —
            // this queue carries operator notifications; a rare duplicate
            // beats a silently dropped reminder. Keep the original promise:
            // a LATE success flips the row to sent before the retry tick
            // re-picks it, suppressing the duplicate entirely. Staged media
            // is unlinked ONLY at terminal state — a retried row needs it.
            const { status } = outbound.markFailure(r.id, e.message, OUTBOUND_MAX_ATTEMPTS);
            consecutiveConnectionFailures += 1; // a hang is rot-shaped
            sendPromise.then(
              (sent) => {
                outbound.markSent(r.id, sent?.key?.id ?? "");
                // By the time the promise resolves Baileys has fully read
                // the file — unlink is safe even on the late path.
                cleanupMedia(r);
                log.warn({ id: r.id }, "whatsapp: timed-out send completed late — marked sent to suppress retry");
              },
              () => { /* already markFailure'd at timeout */ },
            );
            if (status === "failed") cleanupMedia(r);
            log.warn(
              { id: r.id, kind: r.kind, jid, consecutive: consecutiveConnectionFailures },
              "whatsapp: outbound send timed out — aborting tick (hung socket won't recover row-to-row)",
            );
            if (consecutiveConnectionFailures >= CONNECTION_ROT_THRESHOLD) {
              const alert = `⚠️ WhatsApp bot exiting: ${consecutiveConnectionFailures} consecutive failed/hung sendMessage calls — launchd will respawn.`;
              log.error(alert);
              await withTimeout(alertDiscordHome(alert), 5_000).catch(() => {});
              process.exit(1);
            }
            break;
          }
        } catch (e) {
          const err = (e as Error).message;
          const { status } = outbound.markFailure(r.id, err, OUTBOUND_MAX_ATTEMPTS);
          if (status === "failed") cleanupMedia(r);
          log.warn({ id: r.id, err, attempts: r.attempts + 1 }, "whatsapp: outbound send failed");
          if (/connection closed/i.test(err)) {
            consecutiveConnectionFailures += 1;
            log.warn(
              { consecutive: consecutiveConnectionFailures, threshold: CONNECTION_ROT_THRESHOLD },
              "whatsapp: connection-rot counter incremented",
            );
          }
        }
        if (consecutiveConnectionFailures >= CONNECTION_ROT_THRESHOLD) {
          const alert = `⚠️ WhatsApp bot exiting: ${consecutiveConnectionFailures} consecutive failed/hung sendMessage calls — launchd will respawn.`;
          log.error(alert);
          await withTimeout(alertDiscordHome(alert), 5_000).catch(() => {});
          process.exit(1);
        }
      }
    } finally {
      outboundDraining = false;
      drainTickStartedAt = null;
    }
  }, OUTBOUND_DRAIN_INTERVAL_MS);
}

/** Translate a queue row's `target` string to a JID. Three accepted forms:
 *    - Group JID (`@g.us`) — must be in `allowedJids` (resolved at startup).
 *    - DM target — either a full `<digits>@s.whatsapp.net` JID or bare
 *      digits (8-15 chars). Both normalize to the digit form and check
 *      against `config.allowedDmSenders`. Returns the canonical
 *      `<digits>@s.whatsapp.net` shape.
 *    - Group name — resolved via the allowlist's name→JID map.
 *  Returns null if the target isn't authorized — no sending to arbitrary chats. */
function resolveOutboundTarget(target: string, config: Config): string | null {
  // Group JID path.
  if (target.includes("@g.us")) {
    return allowedJids.has(target) ? target : null;
  }
  // DM path: full @s.whatsapp.net JID, or bare digits. The DM allowlist
  // lives in `config.allowedDmSenders` as digit-only strings, not in
  // `allowedJids` (which only holds group JIDs).
  if (target.includes("@s.whatsapp.net") || /^\d{8,15}$/.test(target)) {
    const digits = normalizeSenderId(target);
    if (digits && config.allowedDmSenders.has(digits)) {
      return `${digits}@s.whatsapp.net`;
    }
    return null;
  }
  // Group name path: must be a name we resolved at startup.
  const jid = groupNameToJid.get(target);
  if (!jid) return null;
  return allowedJids.has(jid) ? jid : null;
}

/** Check `participant` against the configured sender allowlist. Modern
 *  WhatsApp groups deliver participants as `@lid` for privacy, so we:
 *    1. compare the LID's user part against the set (zero-cost match),
 *    2. if that misses and the JID is a LID, ask Baileys to resolve
 *       LID → PN via `signalRepository.lidMapping.getPNForLID()` and
 *       compare the resolved PN's user part too.
 *  Returns true when either form is on the list. PN resolution can return
 *  null when the bot hasn't yet seen a mapping for this contact — in
 *  that case the user should put the LID directly in the env (it's
 *  surfaced in the "ignoring" log line). */
async function isSenderAllowed(
  sock: WASocket,
  participant: string,
  allowed: Set<string>,
): Promise<boolean> {
  if (allowed.size === 0) return false;
  const normalized = normalizeSenderId(participant);
  if (normalized && allowed.has(normalized)) return true;
  if (participant.endsWith("@lid")) {
    try {
      const pn: string | null | undefined =
        await sock?.signalRepository?.lidMapping?.getPNForLID?.(participant);
      if (pn) {
        const pnUser = normalizeSenderId(pn);
        if (pnUser && allowed.has(pnUser)) return true;
      }
    } catch {
      // Resolution failure: fall through to deny. The participant LID
      // is still logged in the caller so the operator can add it.
    }
  }
  return false;
}

async function handleMessage(
  sock: WASocket,
  msg: WAMessage,
  config: Config,
  store: ChatSessionStore,
  sessions: SessionPool,
  sessionsDm: SessionPool,
  plansStore: PendingPlansStore,
  docStore: DocStore,
  jobStore: JobStore,
  outbound: OutboundQueueStore,
): Promise<void> {
  const chatId = msg.key.remoteJid;
  if (!chatId) return;

  if (config.discoverMode) {
    const preview = extractText(msg).slice(0, 80);
    log.info({ chatId, fromMe: msg.key.fromMe, preview }, "whatsapp: [discover]");
    return;
  }

  // ---- IRON-TIGHT FILTERS ----
  // 1. Resolve role: groups match by literal JID in `allowedJids`; DMs
  //    match by normalized digit-only chatId user-part against the DM
  //    sender set (handles both @s.whatsapp.net and @lid forms).
  const role = resolveRole(chatId, config);
  if (!role) return;

  // 2. Chat-type sanity. Groups end @g.us; DMs end @s.whatsapp.net or @lid.
  const kind = chatType(chatId);
  if (kind === "group" && !chatId.endsWith("@g.us")) return;

  // 3. Don't reply to ourselves — would loop. Silent skip, not a warn.
  if (msg.key.fromMe) return;

  if (kind === "group") {
    // 4G. Per-sender allowlist. Pre-bot-number-split this gate didn't exist
    //     because bot==user (every legit message was fromMe). Now the bot
    //     runs as a separate identity, so we must explicitly enumerate who
    //     is allowed to address it inside an allowlisted group. Without this,
    //     anyone who creates a group with the same name as one of yours and
    //     adds the bot could spam it.
    const participant = msg.key.participant ?? "";
    const senderOk = await isSenderAllowed(sock, participant, config.allowedSenders);
    if (!senderOk) {
      log.warn(
        { chatId, participant },
        "whatsapp: sender not in WHATSAPP_ALLOWED_SENDERS — ignoring (add the listed participant if this is you)",
      );
      return;
    }

    // 5G. Membership-change tripwire. The sender allowlist defends against
    //     *messages* from the wrong identity; this tripwire still flags
    //     group-composition drift so we notice if someone gets added to an
    //     allowlisted group, even if they never speak.
    let memberIds: string[] = [];
    try {
      const metadata = await sock.groupMetadata(chatId);
      memberIds = metadata.participants.map((p: any) => p.id);
    } catch (e) {
      log.warn({ err: (e as Error).message }, "whatsapp: groupMetadata failed — refusing to respond");
      return;
    }
    const { disabled, reason } = store.observeMembers(chatId, memberIds);
    if (disabled) {
      log.warn({ chatId, reason }, "whatsapp: group disabled — manual re-enable required");
      return;
    }
  } else {
    // 4D. DM path (ADR-005b). The sender == chatId by definition; the JID
    //     is on `allowedDmJids` because role resolution succeeded. No
    //     participant allowlist + no membership tripwire (single-party
    //     chat). The role can't be `braindump` here — we never seed
    //     @s.whatsapp.net JIDs as braindump — but assert defensively.
    if (role !== "dm") {
      log.warn({ chatId, role }, "whatsapp: DM with non-dm role — refusing");
      return;
    }
  }
  // ---- END FILTERS ----

  // ADR-018: inbound media (images/documents) intercepts BEFORE the
  // braindump dispatch — media archives to the document library in every
  // role; only the DM role additionally gets the act-on-this path.
  // documentWithCaptionMessage normalization matters: Baileys delivers
  // captioned documents under that wrapper, and checking only
  // documentMessage silently drops them.
  const inboundDoc =
    msg.message?.documentMessage ??
    msg.message?.documentWithCaptionMessage?.message?.documentMessage;
  const inboundImg = msg.message?.imageMessage;
  if (inboundDoc || inboundImg) {
    await handleInboundMedia(sock, msg, chatId, role, config, docStore, jobStore, outbound);
    return;
  }

  // Brain-dump dispatches before extraction — the role handler decides
  // whether this is a reply (skip transcription, treat text as response)
  // or a new capture (run the planning pipeline). Ack timing also differs
  // between the two paths, so each branch owns its own acks.
  if (role === "braindump") {
    await handleBrainDump(sock, msg, chatId, config, plansStore);
    return;
  }

  // Conversational path: extraction is the same for text + voice.
  let text = "";
  let inputKind: "text" | "voice" = "text";

  if (msg.message?.audioMessage) {
    inputKind = "voice";
    await sock.sendPresenceUpdate("recording", chatId);
    try {
      const buffer = (await downloadMediaMessage(msg, "buffer", {}, {
        logger: baileysLogger as any,
        reuploadRequest: sock.updateMediaMessage,
      })) as Buffer;
      const dur = msg.message.audioMessage.seconds ?? 0;
      log.info({ chatId, bytes: buffer.length, seconds: dur }, "whatsapp: transcribing voice memo");
      const result = await transcribe(buffer);
      text = result.text;
      log.info({ chatId, transcribedChars: text.length, ms: result.durationMs }, "whatsapp: transcribed");
    } catch (e) {
      const err = (e as Error).message;
      log.error({ err }, "whatsapp: transcription failed");
      await sock.sendMessage(chatId, { text: formatReply(`couldn't transcribe that — ${err}`) });
      return;
    }
  } else {
    text = extractText(msg);
  }

  if (!text.trim()) return;

  log.info({ chatId, role, kind: inputKind, len: text.length }, "whatsapp: processing message");

  // Brain-dump capture is structurally group-only: a JID only carries the
  // braindump role if it was seeded from a braindump group/CHAT_ID env var,
  // and DM JIDs (@s.whatsapp.net) never appear in those lists. So there's
  // nothing to reject here — the role split itself enforces it.
  const pool = role === "dm" ? sessionsDm : sessions;
  await handleConversational(sock, chatId, text, inputKind, config, store, pool);
}

async function handleConversational(
  sock: WASocket,
  chatId: string,
  text: string,
  inputKind: "text" | "voice",
  config: Config,
  store: ChatSessionStore,
  sessions: SessionPool,
): Promise<void> {
  await sock.sendPresenceUpdate("composing", chatId);
  try {
    const resume = store.lookup(chatId) ?? undefined;
    const result = await sessions.ask(chatId, frame(text, chatId, inputKind), resume);
    store.save(chatId, result.sessionId, !resume);

    const rawReply = result.reply.trim() || "(no response)";
    const formatted = formatReply(rawReply);
    await sock.sendMessage(chatId, { text: formatted });
    log.info(
      {
        chatId,
        replyChars: rawReply.length,
        sessionId: result.sessionId.slice(0, 8),
        elapsedMs: result.elapsedMs,
        cold: result.wasColdSpawn,
      },
      "whatsapp: reply sent",
    );

    recordDiary(
      config.diaryRoot,
      chatId.endsWith("@g.us") ? "self-group" : "dm",
      `replied to ${inputKind} in ${(result.elapsedMs / 1000).toFixed(1)}s (${text.length}c in → ${rawReply.length}c out, ${result.wasColdSpawn ? "cold" : "warm"} session ${result.sessionId.slice(0, 8)})`,
      "OBSERVATION",
    );

    // On-the-fly skill review (ADR-017) — detached, never blocks the reply.
    if (result.reviewDue) {
      fireSkillReview(config.workspaceRoot, "whatsapp", chatId, result.transcriptPath);
    }
  } catch (e) {
    const err = (e as Error).message;
    log.error({ err }, "whatsapp: claude call failed");
    await sock.sendMessage(chatId, {
      text: formatReply(`handler error:\n\`\`\`\n${err}\n\`\`\``),
    });
  } finally {
    await sock.sendPresenceUpdate("paused", chatId);
  }
}

/** ADR-018 inbound media: archive EVERY image/document to the local
 *  library (dedup absorbs re-sends), ack with the stored name, and — DM
 *  role only, when the caption reads as an instruction — hand the
 *  DOCSTORE PATH to the session to Read (no staging copy: the archived
 *  file is already a local, session-readable path under the workspace;
 *  one copy, one lifecycle). Braindump role is capture-only: archive +
 *  ack, captions become names, never a session ask. */
async function handleInboundMedia(
  sock: WASocket,
  msg: WAMessage,
  chatId: string,
  role: ChatRole,
  config: Config,
  docStore: DocStore,
  jobStore: JobStore,
  outbound: OutboundQueueStore,
): Promise<void> {
  const m = msg.message;
  const docMsg = m?.documentMessage ?? m?.documentWithCaptionMessage?.message?.documentMessage;
  const imgMsg = m?.imageMessage;
  const media = docMsg ?? imgMsg;
  if (!media) return;

  const isImage = !docMsg;
  const caption = (docMsg?.caption ?? imgMsg?.caption ?? "").trim();
  const origName = docMsg?.fileName ?? null;
  const mimetype = media.mimetype ?? (isImage ? "image/jpeg" : "application/octet-stream");

  // Size pre-check before downloading — fileLength is advisory but honest.
  const declared = Number(media.fileLength ?? 0);
  if (declared > config.mediaMaxBytes) {
    await sock.sendMessage(chatId, {
      text: formatReply(
        `that file is ~${Math.round(declared / 1024 / 1024)}MB — over the ${Math.round(config.mediaMaxBytes / 1024 / 1024)}MB library cap, not archiving it`,
      ),
    });
    return;
  }

  let buffer: Buffer;
  try {
    buffer = (await downloadMediaMessage(msg, "buffer", {}, {
      logger: baileysLogger as any,
      reuploadRequest: sock.updateMediaMessage,
    })) as Buffer;
  } catch (e) {
    log.error({ chatId, err: (e as Error).message }, "whatsapp: media download failed");
    await sock.sendMessage(chatId, {
      text: formatReply(`couldn't download that file — ${(e as Error).message}`),
    });
    return;
  }

  const decision = classifyCaption(caption);
  const localToday = new Date().toISOString().slice(0, 10);
  const logicalName =
    decision.name ??
    (origName ? origName.replace(/\.[a-z0-9]{1,8}$/i, "") : `unnamed-${localToday}`);
  const filename = origName ?? `${logicalName}.${isImage ? "jpg" : "bin"}`;

  let record;
  let deduped = false;
  try {
    const res = docStore.add({
      data: buffer,
      logicalName,
      filename,
      mimetype,
      source: role === "braindump" ? "inbound-braindump" : "inbound-dm",
      channel: chatId,
    });
    record = res.record;
    deduped = res.deduped;
  } catch (e) {
    log.error({ chatId, err: (e as Error).message }, "whatsapp: docstore add failed");
    await sock.sendMessage(chatId, {
      text: formatReply(`couldn't archive that file — ${(e as Error).message}`),
    });
    return;
  }

  log.info(
    { chatId, role, id: record.id, name: record.logicalName, bytes: record.bytes, deduped },
    "whatsapp: inbound media archived",
  );

  // ADR-013: auto-enrich every NON-deduped archive (silent; keywords +
  // summary land in documents.db for find()). `priv:` opts out — those
  // bytes never enter any session.
  if (!deduped && !decision.noEnrich) {
    void fireEnrichJob({ jobStore, docStore, config, record, chatId });
  }
  await sock.sendMessage(chatId, {
    text: formatReply(
      `📄 arquivado: ${record.logicalName} (id ${record.id.slice(0, 8)})${deduped ? " — já existia" : ""}`,
    ),
  });

  // Vault-import path (ADR-013, opt-in via vault:/import: caption, DM
  // only): same promotion wrapper as act — extracting a long PDF easily
  // outlives the quick window. runImportJob handles the identity-tag
  // guard internally (returns the refusal line as the reply).
  if (role === "dm" && decision.mode === "vault-import") {
    await sock.sendPresenceUpdate("composing", chatId);
    const dmDigits = normalizeSenderId(chatId);
    const importPromise = runImportJob({ jobStore, docStore, config, record, chatId });
    try {
      const raced = await withQuickWindow(importPromise, ACT_QUICK_WINDOW_MS);
      if (raced.settled) {
        await sock.sendMessage(chatId, { text: formatReply(raced.value) });
      } else {
        await sock.sendMessage(chatId, {
          text: formatReply("recebi, extraindo para o vault — aviso quando terminar 📥"),
        });
        importPromise.then(
          (resultLine) => {
            if (dmDigits) {
              outbound.enqueue({
                target: dmDigits,
                source: "job-import",
                body: formatReply(resultLine),
              });
            }
          },
          (e) => {
            if (dmDigits) {
              outbound.enqueue({
                target: dmDigits,
                source: "job-import",
                body: formatReply(
                  `📥 importação de "${record.logicalName}" falhou:\n\`\`\`\n${(e as Error).message}\n\`\`\``,
                ),
              });
            }
          },
        );
      }
    } catch (e) {
      await sock.sendMessage(chatId, {
        text: formatReply(
          `arquivei, mas a importação falhou:\n\`\`\`\n${(e as Error).message}\n\`\`\``,
        ),
      });
    } finally {
      await sock.sendPresenceUpdate("paused", chatId);
    }
    return;
  }

  // Act path: DM only, instruction-shaped caption. ADR-013: runs on a
  // ONE-SHOT JOB SESSION (own tmux session) — the per-chat DM lock is
  // never taken, so a 3-minute analysis can't block your next message.
  // Timeout promotion: answer within ACT_QUICK_WINDOW_MS → single direct
  // reply; else ack now, deliver the result via the outbound queue when
  // the job finishes. Trade documented in ADR-013: act replies don't
  // share the DM session's conversational memory (the doc + instruction
  // are self-contained; the DM pool can docs.ts-find the doc for
  // follow-ups).
  if (role === "dm" && decision.mode === "act" && decision.instruction) {
    const docPath = docStore.pathFor(record);
    const framed = `[attached ${isImage ? "image" : "document"} "${record.logicalName}" archived at ${docPath} — use the Read tool on it]

${decision.instruction}`;
    await sock.sendPresenceUpdate("composing", chatId);
    // Deferred-delivery target MUST be digits — raw @lid chatIds fail the
    // drain's resolveOutboundTarget silently into failed rows.
    const dmDigits = normalizeSenderId(chatId);
    const { jobId, promise } = startJob({
      store: jobStore,
      config,
      kind: "act",
      chatId,
      docId: record.id,
      instruction: decision.instruction.slice(0, 200),
      prompt: framed,
      appendSystemPrompt: JOB_ACT_SYSTEM_PROMPT,
      allowedTools: [
        "Bash(npx --prefix messaging/whatsapp tsx messaging/whatsapp/src/docs.ts:*)",
      ],
    });
    try {
      const raced = await withQuickWindow(promise, ACT_QUICK_WINDOW_MS);
      if (raced.settled) {
        const rawReply = raced.value.reply.trim() || "(no response)";
        await sock.sendMessage(chatId, { text: formatReply(rawReply) });
        log.info(
          { chatId, jobId, id: record.id, elapsedMs: raced.value.elapsedMs },
          "whatsapp: act-on-media replied in-window",
        );
      } else {
        jobStore.markPromoted(jobId);
        await sock.sendMessage(chatId, {
          text: formatReply("recebi, analisando — respondo já 📄"),
        });
        log.info({ chatId, jobId, id: record.id }, "whatsapp: act-on-media promoted to deferred job");
        // Deferred path: result (or failure) arrives via the queue. The
        // queue body is sent raw by the drain, so formatReply here.
        promise.then(
          (outcome) => {
            if (dmDigits) {
              outbound.enqueue({
                target: dmDigits,
                source: "job-act",
                body: formatReply(`📄 ${record.logicalName}:\n${outcome.reply.trim()}`),
              });
            }
          },
          (e) => {
            if (dmDigits) {
              outbound.enqueue({
                target: dmDigits,
                source: "job-act",
                body: formatReply(
                  `📄 ${record.logicalName} — análise falhou:\n\`\`\`\n${(e as Error).message}\n\`\`\``,
                ),
              });
            }
          },
        );
      }
    } catch (e) {
      // In-window failure: reply directly (the job row is already marked
      // failed by the runner).
      const err = (e as Error).message;
      log.error({ chatId, jobId, err }, "whatsapp: act-on-media job failed in-window");
      await sock.sendMessage(chatId, {
        text: formatReply(`arquivei, mas não consegui processar o pedido:\n\`\`\`\n${err}\n\`\`\``),
      });
    } finally {
      await sock.sendPresenceUpdate("paused", chatId);
    }
  }
}

/** Fire a detached on-the-fly skill review (ADR-017). Best-effort and fully
 *  decoupled — shells out to the built skill-gap-learner binary and returns
 *  immediately so it never blocks the reply. No-op if the binary isn't built. */
function fireSkillReview(
  workspaceRoot: string,
  venue: string,
  chatKey: string,
  transcriptPath: string,
): void {
  const release = path.join(workspaceRoot, "target/release/skill-gap-learner");
  const debug = path.join(workspaceRoot, "target/debug/skill-gap-learner");
  const bin = fs.existsSync(release) ? release : fs.existsSync(debug) ? debug : null;
  if (!bin) return;
  try {
    const child = spawn(
      bin,
      ["review", "--transcript", transcriptPath, "--venue", venue, "--chat-key", chatKey],
      { cwd: workspaceRoot, detached: true, stdio: "ignore" },
    );
    child.unref();
  } catch {
    /* best-effort */
  }
}

/** Brain-dump pipeline entry (ADR-005a review-before-apply).
 *
 *  Voice memos are always new captures (auto-expire prior plan, transcribe,
 *  plan). Text messages route based on pending-plan state:
 *    - pending plan exists → handlePlanResponse (interpret reply)
 *    - no pending plan      → handleNewCapture (run planning)
 *
 *  Each branch owns its own ack cadence.
 */
async function handleBrainDump(
  sock: WASocket,
  msg: WAMessage,
  chatId: string,
  config: Config,
  plansStore: PendingPlansStore,
): Promise<void> {
  // Voice memos can't realistically be a reply to a structured plan, so we
  // treat them as new captures unconditionally. A prior pending plan (if
  // any) is expired with a notice.
  if (msg.message?.audioMessage) {
    await sendBotAck(sock, chatId, "✓ recebido");
    const dur = msg.message.audioMessage.seconds ?? 0;
    await sendBotAck(sock, chatId, `🎧 transcrevendo memo de ${dur}s…`);

    let text: string;
    try {
      const buffer = (await downloadMediaMessage(msg, "buffer", {}, {
        logger: baileysLogger as any,
        reuploadRequest: sock.updateMediaMessage,
      })) as Buffer;
      log.info({ chatId, bytes: buffer.length, seconds: dur }, "whatsapp: transcribing voice memo");
      const result = await transcribe(buffer);
      text = result.text;
      log.info({ chatId, transcribedChars: text.length, ms: result.durationMs }, "whatsapp: transcribed");
    } catch (e) {
      const err = (e as Error).message;
      log.error({ err }, "whatsapp: transcription failed");
      await sock.sendMessage(chatId, { text: formatReply(`couldn't transcribe that — ${err}`) });
      return;
    }
    if (!text.trim()) return;
    await expireAnyPendingPlan(sock, chatId, plansStore);
    await handleNewCapture(sock, chatId, text, "voice", config, plansStore);
    return;
  }

  const text = extractText(msg);
  if (!text.trim()) return;

  const pending = plansStore.mostRecentPending(chatId);
  if (pending) {
    await handlePlanResponse(sock, chatId, text, pending, config, plansStore);
  } else {
    await sendBotAck(sock, chatId, "✓ recebido");
    await handleNewCapture(sock, chatId, text, "text", config, plansStore);
  }
}

/** New-capture path: spawn planning Claude session (which sends its own
 *  🧠 ack via src/ack.ts), receive plan, send rundown. The plan row is
 *  persisted inside planCapture; the operator's reply will be handled by
 *  a subsequent inbound message → handlePlanResponse. */
async function handleNewCapture(
  sock: WASocket,
  chatId: string,
  text: string,
  inputKind: "text" | "voice",
  config: Config,
  plansStore: PendingPlansStore,
): Promise<void> {
  await sock.sendPresenceUpdate("composing", chatId);
  let plan: PlanForReview;
  try {
    plan = await planCapture(text, inputKind, config, chatId, plansStore);
  } catch (e) {
    const err = (e as Error).message;
    log.error({ err }, "whatsapp: braindump planning failed");
    await sock.sendMessage(chatId, { text: formatReply(`couldn't plan that — ${err}`) });
    await sock.sendPresenceUpdate("paused", chatId);
    return;
  }

  // No-op plan: Claude decided nothing needs filing (e.g. capture was a
  // meta-test, or the operator said "ignore this"). Skip the rundown +
  // review cycle entirely — there's nothing to approve. Just confirm
  // and resolve. Avoids the silly "plano #X / (no ops) / responda em
  // texto livre" message that asks for a reply with nothing to reply about.
  if (plan.ops.length === 0) {
    plansStore.resolve(plan.planId, "applied", "no-op plan (claude returned 0 ops)");
    await sock.sendMessage(chatId, { text: formatReply("✓ nada para arquivar") });
    await sock.sendPresenceUpdate("paused", chatId);
    log.info(
      { chatId, planId: plan.shortId, summary: plan.summary, elapsedMs: plan.elapsedMs },
      "whatsapp: braindump no-op plan auto-resolved",
    );
    recordDiary(
      config.diaryRoot,
      "braindump",
      `plan #${plan.shortId} no-op from ${inputKind} (${text.length}c): ${plan.summary || "(no summary)"}`,
      "OBSERVATION",
    );
    return;
  }

  await sock.sendMessage(chatId, { text: formatReply(formatRundown(plan)) });
  await sock.sendPresenceUpdate("paused", chatId);

  log.info(
    {
      chatId,
      planId: plan.shortId,
      ops: plan.ops.length,
      confidence: plan.confidence,
      elapsedMs: plan.elapsedMs,
    },
    "whatsapp: braindump plan ready, awaiting review",
  );
  recordDiary(
    config.diaryRoot,
    "braindump",
    `plan #${plan.shortId} from ${inputKind} (${text.length}c) → ${plan.summary} (${(plan.confidence * 100).toFixed(0)}% conf, ${(plan.elapsedMs / 1000).toFixed(1)}s, ${plan.ops.length} ops; awaiting review)`,
    "OBSERVATION",
  );
}

/** Plan-response path: spawn response-interpreter, branch on action. */
async function handlePlanResponse(
  sock: WASocket,
  chatId: string,
  replyText: string,
  pending: ReturnType<PendingPlansStore["mostRecentPending"]> & {},
  config: Config,
  plansStore: PendingPlansStore,
): Promise<void> {
  await sendBotAck(sock, chatId, "⚙️ interpretando…");
  await sock.sendPresenceUpdate("composing", chatId);

  let result: import("./braindump.js").InterpretResult;
  try {
    result = await interpretResponse(pending, replyText, config);
  } catch (e) {
    const err = (e as Error).message;
    log.error({ err, planId: shortPlanId(pending.id) }, "whatsapp: interpret failed");
    await sock.sendMessage(chatId, { text: formatReply(`erro interpretando: ${err}`) });
    await sock.sendPresenceUpdate("paused", chatId);
    return;
  }

  const shortId = shortPlanId(pending.id);
  log.info({ chatId, planId: shortId, action: result.action, ids: result.ids }, "whatsapp: braindump interpret");

  if (result.action === "ambiguous") {
    const note = result.note ?? "não entendi, pode reformular?";
    await sock.sendMessage(chatId, { text: formatReply(note) });
    await sock.sendPresenceUpdate("paused", chatId);
    return;
  }

  if (result.action === "reject") {
    plansStore.resolve(pending.id, "rejected", result.note ?? "operator rejected");
    await sock.sendMessage(chatId, { text: formatReply(`✓ plano #${shortId} cancelado`) });
    await sock.sendPresenceUpdate("paused", chatId);
    recordDiary(
      config.diaryRoot,
      "braindump",
      `plan #${shortId} rejected by operator${result.note ? ` (${result.note})` : ""}`,
      "OBSERVATION",
    );
    return;
  }

  if (result.action === "new_capture") {
    // Operator sent fresh content instead of replying. Expire this plan
    // and re-process the message as a new capture.
    plansStore.resolve(pending.id, "expired", "superseded by new capture");
    await sock.sendMessage(chatId, {
      text: formatReply(`⏱ plano #${shortId} cancelado — processando novo capture`),
    });
    await sock.sendPresenceUpdate("paused", chatId);
    await handleNewCapture(sock, chatId, replyText, "text", config, plansStore);
    return;
  }

  // action === "apply" | "modify". `modify` carries field-level patches
  // (a placement/naming correction the operator made at review) which
  // applyPlan applies to the ops before filing — so a date/bucket/rename
  // fix files the same turn instead of cancelling the plan.
  await sendBotAck(sock, chatId, result.action === "modify" ? "✏️ corrigindo e aplicando…" : "📂 aplicando…");
  let outcome: import("./braindump.js").CaptureOutcome;
  try {
    outcome = applyPlan(pending.id, result.ids ?? "all", plansStore, config, result.patches ?? []);
  } catch (e) {
    const err = (e as Error).message;
    log.error({ err, planId: shortId }, "whatsapp: applyPlan failed");
    await sock.sendMessage(chatId, { text: formatReply(`erro aplicando: ${err}`) });
    await sock.sendPresenceUpdate("paused", chatId);
    return;
  }
  const reply = formatOutcomeReply(outcome.summary, outcome.confidence, outcome.ops);
  await sock.sendMessage(chatId, { text: formatReply(reply) });
  await sock.sendPresenceUpdate("paused", chatId);

  log.info(
    {
      chatId,
      planId: shortId,
      ops: outcome.ops.length,
      ok: outcome.ops.filter((o) => o.status === "ok").length,
      rejected: outcome.ops.filter((o) => o.status === "rejected").length,
      elapsedMs: outcome.elapsedMs,
    },
    "whatsapp: braindump plan applied",
  );
  recordDiary(
    config.diaryRoot,
    "braindump",
    `plan #${shortId} applied (${outcome.ops.filter((o) => o.status === "ok").length}/${outcome.ops.length} ok)`,
    "OBSERVATION",
  );
}

/** Expire any pending plan for this chat, notifying the operator. Called
 *  on voice-memo arrival (and as a defensive sweep before new captures)
 *  so a stale plan doesn't compete with the new one. */
async function expireAnyPendingPlan(
  sock: WASocket,
  chatId: string,
  plansStore: PendingPlansStore,
): Promise<void> {
  const expired = plansStore.expirePendingForChat(chatId, "superseded by new capture");
  for (const id of expired) {
    const sid = shortPlanId(id);
    await sock.sendMessage(chatId, {
      text: formatReply(`⏱ plano #${sid} cancelado — processando novo capture`),
    }).catch((e: Error) =>
      log.warn({ err: e.message, planId: sid }, "whatsapp: cancel notice failed"),
    );
  }
}

/** Send a short status ack with the venue's signature applied. */
async function sendBotAck(sock: WASocket, chatId: string, body: string): Promise<void> {
  await sock.sendMessage(chatId, { text: formatReply(body) }).catch((e: Error) =>
    log.warn({ err: e.message }, "whatsapp: ack send failed"),
  );
}

/** Periodic sweep: expire `pending_plans` rows older than PLAN_TIMEOUT_MS
 *  and notify each affected chat. Handles the "operator walked away"
 *  case where no inbound traffic triggers the on-entry sweep. */
function startPlanExpirySweep(sock: WASocket, plansStore: PendingPlansStore): void {
  if (planExpirySweepTimer) clearInterval(planExpirySweepTimer);
  planExpirySweepTimer = setInterval(async () => {
    let rows: ReturnType<PendingPlansStore["sweepExpired"]>;
    try {
      rows = plansStore.sweepExpired(PLAN_TIMEOUT_MS);
    } catch (e) {
      log.warn({ err: (e as Error).message }, "whatsapp: plan sweep failed");
      return;
    }
    if (rows.length === 0) return;
    log.info({ count: rows.length }, "whatsapp: swept expired braindump plans");
    for (const row of rows) {
      const sid = shortPlanId(row.id);
      await sock.sendMessage(row.chatId, {
        text: formatReply(`⏱ plano #${sid} expirou — reenvie se ainda quiser`),
      }).catch((e: Error) =>
        log.warn({ err: e.message, planId: sid }, "whatsapp: expiry notice failed"),
      );
    }
  }, PLAN_SWEEP_INTERVAL_MS);
}

/** Format a multi-op outcome as a human-readable WhatsApp reply.
 *
 *  Format:
 *    <summary> (<conf>% confidence)
 *
 *    + 3-Projects/Example-Project/contract.md
 *    + 3-Projects/Example-Project/team.md
 *    ↑ 4-Areas/Career/relationships.md (appended)
 *    → 3-Projects/Example-Project/overview.md (moved from 0-Inbox/old.md)
 *    ✗ 3-Projects/X (rejected: sub-folder X doesn't exist)
 *
 *  Glyphs are a small dialect: + = create, ↑ = append, → = move,
 *  ✗ = rejected. Reads well in WhatsApp's monospace renderer.
 */
function formatOutcomeReply(
  summary: string,
  confidence: number,
  ops: AppliedOp[],
): string {
  const conf = (confidence * 100).toFixed(0);
  const lines: string[] = [`${summary} (${conf}% confidence)`, ""];
  for (const op of ops) {
    if (op.status === "rejected") {
      lines.push(`✗ ${op.op} rejected: ${op.rejection ?? "(no reason)"}`);
      continue;
    }
    switch (op.op) {
      case "create":
        lines.push(`+ ${op.resultPath}`);
        break;
      case "append":
        lines.push(`↑ ${op.resultPath} (appended)`);
        break;
      case "move":
        lines.push(`→ ${op.resultPath} (moved from ${op.fromPath})`);
        break;
    }
  }
  return lines.join("\n");
}

/** Persona display name on every outbound message. Code identity stays
 *  venue-based (Rule 7); the persona's user-facing name comes from the
 *  resolved persona's `display_name` frontmatter (ADR-009), which lives
 *  in the persona file's frontmatter rather than env. Initialized at boot
 *  by `configurePersona`; defaults to `"bot"` if a handler somehow runs
 *  before config is loaded. */
let personaDisplayName = "bot";

export function configurePersona(displayName: string): void {
  personaDisplayName = displayName;
}

/** Format every outbound message so it's distinguishable from the user's
 *  own typed messages in the same self-group. Thin wrapper over the shared
 *  implementation, bound to the boot-time persona. */
function formatReply(body: string): string {
  return sharedFormatReply(body, personaDisplayName);
}

function extractText(msg: WAMessage): string {
  const m = msg.message;
  if (!m) return "";
  if (m.conversation) return m.conversation;
  if (m.extendedTextMessage?.text) return m.extendedTextMessage.text;
  if (m.imageMessage?.caption) return m.imageMessage.caption;
  if (m.videoMessage?.caption) return m.videoMessage.caption;
  return "";
}

function frame(text: string, chatId: string, kind: "text" | "voice"): string {
  const header = kind === "voice"
    ? `[WhatsApp voice memo — chat ${chatId}, transcribed]`
    : `[WhatsApp — chat ${chatId}]`;
  return `${header}\n\n${text}`;
}

main().catch((e) => {
  log.fatal(e, "whatsapp: fatal");
  process.exit(1);
});
