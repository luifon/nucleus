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
import { ChatSessionStore, OutboundQueueStore, type OutboundRow } from "./db.js";
import { record as recordDiary } from "./diary.js";
import { transcribe } from "./transcribe.js";
import { captureToPara, type AppliedOp } from "./braindump.js";

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
 *   "alfred"    — conversational. Send a message, get a reply via SessionPool.
 *                 Voice memos are transcribed → asked → reply.
 *   "braindump" — capture-only. Inbound messages get classified and filed
 *                 into the PARA-organized vault (T3). Voice memos are
 *                 transcribed → filed as PARA notes. The bot may reply
 *                 with confirmation ("filed to 2-Areas/Nucleus/...") or
 *                 escalate when classification is uncertain (Phase 4).
 *
 * Built at connection.open from the four allowlist sources in Config
 * (alfred chatIds + groupNames, braindump chatIds + groupNames). Empty
 * until populated. Held in module scope so the message handler can read it
 * without plumbing through args.
 */
type ChatRole = "alfred" | "braindump";
const allowedJids = new Map<string, ChatRole>();

/** Reverse lookup populated by resolveAllowlist: group name (case-
 *  sensitive, as configured in .env) → JID. Used by the outbound queue
 *  drainer to translate "Alfred" / "Brain Dump" targets into JIDs. */
const groupNameToJid = new Map<string, string>();

const OUTBOUND_DRAIN_INTERVAL_MS = 5_000;
const OUTBOUND_MAX_ATTEMPTS = 5;

async function main() {
  const discover = process.argv.includes("--discover");
  const workspaceRoot =
    process.env.NUCLEUS_WORKSPACE_ROOT ??
    path.resolve(import.meta.dirname, "..", "..", "..");
  const config = loadConfig(workspaceRoot, discover);

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
    appendSystemPrompt: config.appendSystemPrompt,
    permissionMode: config.permissionMode,
    disallowedTools: config.disallowedTools,
    tmuxSession: "nucleus-whatsapp",
    idleTimeoutMs: 4 * 60 * 60 * 1000, // 4h
  });

  // Background idle reaper.
  setInterval(async () => {
    try {
      const n = await sessions.reapIdle();
      if (n > 0) log.info({ reaped: n }, "whatsapp: reaped idle sessions");
    } catch (e) {
      log.warn({ err: (e as Error).message }, "whatsapp: reap failed");
    }
  }, 30 * 60 * 1000);

  await connect(config, store, sessions, outbound);
}

async function connect(
  config: Config,
  store: ChatSessionStore,
  sessions: SessionPool,
  outbound: OutboundQueueStore,
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
        .then(() => startOutboundDrain(sock, outbound))
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
        setTimeout(() => connect(config, store, sessions, outbound).catch((e) => log.error(e, "reconnect failed")), delay);
      } else {
        log.error("whatsapp: logged out — delete auth/ and re-pair");
      }
    }
  });

  sock.ev.on("messages.upsert", async ({ messages, type }) => {
    if (type !== "notify") return;
    for (const msg of messages) {
      await handleMessage(sock, msg, config, store, sessions).catch((e) => {
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
  for (const jid of config.allowedChatIds) allowedJids.set(jid, "alfred");
  for (const jid of config.brainDumpChatIds) allowedJids.set(jid, "braindump");

  const wantAlfred = new Set(config.allowedGroupNames.map((n) => n.toLowerCase()));
  const wantBrainDump = new Set(config.brainDumpGroupNames.map((n) => n.toLowerCase()));
  if (!wantAlfred.size && !wantBrainDump.size) {
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
      } else if (wantAlfred.has(lower)) {
        allowedJids.set(jid, "alfred");
        groupNameToJid.set(name, jid);
        matches.push({ jid, name, role: "alfred" });
      }
    }
    log.info(
      {
        requestedAlfred: config.allowedGroupNames,
        requestedBrainDump: config.brainDumpGroupNames,
        matched: matches,
        allowedJids: Object.fromEntries(allowedJids),
      },
      "whatsapp: allowlist resolved",
    );
    const totalRequested = wantAlfred.size + wantBrainDump.size;
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
function startOutboundDrain(sock: any, outbound: OutboundQueueStore): void {
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
      const jid = resolveOutboundTarget(r.target);
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

/** Translate a queue row's `target` string to a JID. Accepts either a
 *  group name (resolved via the allowlist's name→JID map) or a literal
 *  JID (used directly if and only if it's on the allowlist). Returns
 *  null if the target isn't authorized — no sending to arbitrary chats. */
function resolveOutboundTarget(target: string): string | null {
  // Literal JID path: must already be on the allowlist.
  if (target.includes("@g.us") || target.includes("@s.whatsapp.net")) {
    return allowedJids.has(target) ? target : null;
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
): Promise<void> {
  const chatId = msg.key.remoteJid;
  if (!chatId) return;

  if (config.discoverMode) {
    const preview = extractText(msg).slice(0, 80);
    log.info({ chatId, fromMe: msg.key.fromMe, preview }, "whatsapp: [discover]");
    return;
  }

  // ---- IRON-TIGHT FILTERS ----
  // 1. Allowlist must be in the resolved map (env-derived JIDs ∪ name-matched groups).
  const role = allowedJids.get(chatId);
  if (!role) return;

  // 2. Groups only — no DMs, no broadcasts, no channels.
  if (!chatId.endsWith("@g.us")) {
    log.warn({ chatId }, "whatsapp: chatId in allowlist but not a group — ignoring");
    return;
  }

  // 3. Don't reply to ourselves — would loop. Silent skip, not a warn.
  if (msg.key.fromMe) {
    return;
  }

  // 4. Per-sender allowlist. Pre-bot-number-split this gate didn't exist
  //    because bot==user (every legit message was fromMe). Now the bot
  //    runs as a separate identity, so we must explicitly enumerate who
  //    is allowed to address it inside an allowlisted group. Without this,
  //    anyone who creates a group with a colliding name (`Alfred`,
  //    `Brain Dump`) and adds the bot could spam it.
  const participant = msg.key.participant ?? "";
  const senderOk = await isSenderAllowed(sock, participant, config.allowedSenders);
  if (!senderOk) {
    log.warn(
      { chatId, participant },
      "whatsapp: sender not in WHATSAPP_ALLOWED_SENDERS — ignoring (add the listed participant if this is you)",
    );
    return;
  }

  // 5. Membership-change tripwire. The sender allowlist defends against
  //    *messages* from the wrong identity; this tripwire still flags
  //    group-composition drift so we notice if someone gets added to an
  //    Alfred/Brain Dump group, even if they never speak.
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
  // ---- END FILTERS ----

  // Resolve the message body — text OR transcribed voice memo.
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

  if (role === "braindump") {
    await handleBrainDump(sock, chatId, text, inputKind, config);
  } else {
    await handleAlfredConversational(sock, chatId, text, inputKind, config, store, sessions);
  }
}

async function handleAlfredConversational(
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

/** Brain-dump capture pipeline (multi-op).
 *
 *  Captures may decompose into multiple files across multiple folders,
 *  append to existing notes, and include moves of prior misfiled notes.
 *  Sees the vault as --add-dir; returns a list of ops; each op is
 *  validated (path-escape, vault containment, sub-folder gating) and
 *  applied. See ADR-005 + CLAUDE.md Rule 9.
 *
 *  The bot doesn't escalate — if Claude is uncertain, it falls back to
 *  0-Inbox (or whatever safe path the plan chose). Corrections happen
 *  via FOLLOW-UP captures: the user sends "that should be in Projects/X"
 *  and the next plan emits a `move` op against the prior file.
 */
async function handleBrainDump(
  sock: any,
  chatId: string,
  text: string,
  inputKind: "text" | "voice",
  config: Config,
): Promise<void> {
  await sock.sendPresenceUpdate("composing", chatId);
  try {
    const outcome = await captureToPara(text, inputKind, config);
    const reply = formatOutcomeReply(outcome.summary, outcome.confidence, outcome.ops);
    await sock.sendMessage(chatId, { text: formatReply(reply) });
    log.info(
      {
        chatId,
        ops: outcome.ops.length,
        ok: outcome.ops.filter((o) => o.status === "ok").length,
        rejected: outcome.ops.filter((o) => o.status === "rejected").length,
        confidence: outcome.confidence,
        elapsedMs: outcome.elapsedMs,
      },
      "whatsapp: braindump applied",
    );
    recordDiary(
      config.diaryRoot,
      "braindump",
      `captured ${inputKind} (${text.length}c) → ${outcome.summary} (${(outcome.confidence * 100).toFixed(0)}% conf, ${(outcome.elapsedMs / 1000).toFixed(1)}s)`,
      "OBSERVATION",
    );
  } catch (e) {
    const err = (e as Error).message;
    log.error({ err }, "whatsapp: braindump filing failed");
    await sock.sendMessage(chatId, {
      text: formatReply(`couldn't file that — ${err}`),
    });
  } finally {
    await sock.sendPresenceUpdate("paused", chatId);
  }
}

/** Format a multi-op outcome as a human-readable WhatsApp reply.
 *
 *  Format:
 *    <summary> (<conf>% confidence)
 *
 *    + 1-Projects/Example-Project/contract.md
 *    + 1-Projects/Example-Project/team.md
 *    ↑ 2-Areas/Career/relationships.md (appended)
 *    → 1-Projects/Example-Project/overview.md (moved from 0-Inbox/old.md)
 *    ✗ 1-Projects/X (rejected: sub-folder X doesn't exist)
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

/** Persona signature on every outbound message. The name is the character
 *  the bot speaks as (see persona.md). Code identity stays venue-based;
 *  this literal is the only place the persona name appears in code. */
const PERSONA_SIGNATURE = "— Alfred";

/** Format every outbound message so it's distinguishable from the user's
 *  own typed messages in the same self-group: bold body + persona signature. */
function formatReply(body: string): string {
  // WhatsApp bold uses single asterisks and DOES NOT cross newlines reliably.
  // Wrap each non-empty line individually.
  const bolded = body
    .split("\n")
    .map((line) => (line.trim() ? `*${line}*` : line))
    .join("\n");
  return `${bolded}\n\n*${PERSONA_SIGNATURE}*`;
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
