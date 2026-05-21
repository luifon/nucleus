import {
  default as makeWASocket,
  useMultiFileAuthState,
  fetchLatestWaWebVersion,
  makeCacheableSignalKeyStore,
  downloadMediaMessage,
  Browsers,
  DisconnectReason,
  type WAMessage,
} from "@whiskeysockets/baileys";
import { Boom } from "@hapi/boom";
import pino from "pino";
import qrcodeTerminal from "qrcode-terminal";
import * as qrcodeImg from "qrcode";
import { spawn } from "node:child_process";
import path from "node:path";
import fs from "node:fs";

import { loadConfig, normalizeSenderId, type Config } from "./config.js";
import { SessionPool } from "./claude_session.js";
import {
  ChatSessionStore,
  OutboundQueueStore,
  PendingPlansStore,
  shortPlanId,
  type OutboundRow,
} from "./db.js";
import { record as recordDiary } from "./diary.js";
import { transcribe } from "./transcribe.js";
import {
  planCapture,
  applyPlan,
  interpretResponse,
  formatRundown,
  type AppliedOp,
  type PlanForReview,
} from "./braindump.js";

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

// ADR-005a: braindump plan timeout + sweep cadence.
const PLAN_TIMEOUT_MS = 30 * 60 * 1000;
const PLAN_SWEEP_INTERVAL_MS = 5 * 60 * 1000;

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
  // ADR-005a: holds brain-dump plans pending operator review.
  const plansStore = new PendingPlansStore(config.dbPath);

  // Tear down any leftover tmux sessions from a previous run before we own
  // fresh windows. Both the conversational SessionPool (nucleus-whatsapp)
  // and the brain-dump one-shot sessions (nucleus-whatsapp-braindump) get
  // wiped — startup is the safe time to clean orphans from prior crashes.
  for (const sessionName of ["nucleus-whatsapp", "nucleus-whatsapp-braindump"]) {
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
    tmuxSession: "nucleus-whatsapp",
    idleTimeoutMs: 4 * 60 * 60 * 1000, // 4h
  });

  // ADR-005b: a second pool with the DM-context persona. Group JIDs end
  // `@g.us` and DM JIDs end `@s.whatsapp.net`, so the keys never collide
  // — we use chatId as the key in both pools, and the dispatch chooses
  // which pool to talk to based on chatType.
  const sessionsDm = new SessionPool({
    workspaceRoot: config.workspaceRoot,
    appendSystemPrompt: config.appendSystemPromptDm,
    permissionMode: config.permissionMode,
    disallowedTools: config.disallowedTools,
    tmuxSession: "nucleus-whatsapp-dm",
    idleTimeoutMs: 4 * 60 * 60 * 1000, // 4h
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

  await connect(config, store, sessions, sessionsDm, outbound, plansStore);
}

async function connect(
  config: Config,
  store: ChatSessionStore,
  sessions: SessionPool,
  sessionsDm: SessionPool,
  outbound: OutboundQueueStore,
  plansStore: PendingPlansStore,
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
        setTimeout(() => connect(config, store, sessions, sessionsDm, outbound, plansStore).catch((e) => log.error(e, "reconnect failed")), delay);
      } else {
        log.error("whatsapp: logged out — delete auth/ and re-pair");
      }
    }
  });

  sock.ev.on("messages.upsert", async ({ messages, type }) => {
    if (type !== "notify") return;
    for (const msg of messages) {
      await handleMessage(sock, msg, config, store, sessions, sessionsDm, plansStore).catch((e) => {
        log.error({ err: e?.message }, "whatsapp: handler failed");
      });
    }
  });
}

async function resolveAllowlist(sock: any, config: Config): Promise<void> {
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
    for (const [jid, meta] of Object.entries(groups) as [string, any][]) {
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
function startOutboundDrain(sock: any, outbound: OutboundQueueStore, config: Config): void {
  setInterval(async () => {
    let rows: OutboundRow[];
    try {
      rows = outbound.pending(20);
    } catch (e) {
      log.warn({ err: (e as Error).message }, "whatsapp: outbound pending() failed");
      return;
    }
    if (rows.length === 0) return;
    log.info({ count: rows.length }, "whatsapp: draining outbound queue");
    for (const r of rows) {
      const jid = resolveOutboundTarget(r.target, config);
      if (!jid) {
        outbound.markFailure(r.id, `unknown target: ${r.target}`, OUTBOUND_MAX_ATTEMPTS);
        log.warn({ id: r.id, target: r.target }, "whatsapp: outbound target not in allowlist — failed");
        continue;
      }
      try {
        const sent = await sock.sendMessage(jid, { text: r.body });
        outbound.markSent(r.id, sent?.key?.id ?? "");
        log.info({ id: r.id, target: r.target, jid }, "whatsapp: outbound sent");
      } catch (e) {
        const err = (e as Error).message;
        outbound.markFailure(r.id, err, OUTBOUND_MAX_ATTEMPTS);
        log.warn({ id: r.id, err, attempts: r.attempts + 1 }, "whatsapp: outbound send failed");
      }
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
  sock: any,
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
  sock: any,
  msg: WAMessage,
  config: Config,
  store: ChatSessionStore,
  sessions: SessionPool,
  sessionsDm: SessionPool,
  plansStore: PendingPlansStore,
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
  sock: any,
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
  sock: any,
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
  sock: any,
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
  sock: any,
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

  // action === "apply"
  await sendBotAck(sock, chatId, "📂 aplicando…");
  let outcome: import("./braindump.js").CaptureOutcome;
  try {
    outcome = applyPlan(pending.id, result.ids ?? "all", plansStore, config);
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
  sock: any,
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
async function sendBotAck(sock: any, chatId: string, body: string): Promise<void> {
  await sock.sendMessage(chatId, { text: formatReply(body) }).catch((e: Error) =>
    log.warn({ err: e.message }, "whatsapp: ack send failed"),
  );
}

/** Periodic sweep: expire `pending_plans` rows older than PLAN_TIMEOUT_MS
 *  and notify each affected chat. Handles the "operator walked away"
 *  case where no inbound traffic triggers the on-entry sweep. */
function startPlanExpirySweep(sock: any, plansStore: PendingPlansStore): void {
  setInterval(async () => {
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
 *  own typed messages in the same self-group: bold body + persona signature. */
function formatReply(body: string): string {
  // WhatsApp bold uses single asterisks and DOES NOT cross newlines reliably.
  // Wrap each non-empty line individually.
  const bolded = body
    .split("\n")
    .map((line) => (line.trim() ? `*${line}*` : line))
    .join("\n");
  return `${bolded}\n\n*— ${personaDisplayName}*`;
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
