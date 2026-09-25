// One-shot send. Reuses the paired auth state, opens a connection, sends a
// single message, and exits cleanly.
//
// Usage: npm run send -- <target> <message...>
//   target: the operator's DM (digits of a WHATSAPP_ALLOWED_DM_JIDS entry, or
//           its JID) or a configured group (its JID or its name). Any other
//           target is refused before a connection opens, whoever the caller
//           is (target_policy.ts, ADR-033).
//
// The bot identity is whatever account is currently paired. The message
// appears as that account sent it.

import {
  default as makeWASocket,
  useMultiFileAuthState,
  fetchLatestWaWebVersion,
  makeCacheableSignalKeyStore,
  Browsers,
} from "@whiskeysockets/baileys";
import pino from "pino";
import path from "node:path";
import { loadConfig } from "./config.js";
import { refuseUnlessAllowed } from "./caller_guard.js";
import { enqueueRefusal, GroupAllowlist, isOperatorDm, resolveTarget } from "./target_policy.js";

const log = pino({ level: process.env.NUCLEUS_LOG ?? "info" });
const baileysLogger = pino({ level: "silent" });

/** Exit with status 3: the target is not the operator's DM or a
 *  configured group. */
function refuseTarget(target: string, reason: string): never {
  console.error(`send: refused — ${JSON.stringify(target)} is not the operator's DM or a configured group: ${reason} (ADR-033).`);
  process.exit(3);
}

async function main() {
  const workspaceRoot =
    process.env.NUCLEUS_WORKSPACE_ROOT ??
    path.resolve(import.meta.dirname, "..", "..", "..");
  const config = loadConfig(workspaceRoot, false);
  const args = process.argv.slice(2);
  if (args.length < 2) {
    console.error("usage: npm run send -- <target> <message...>");
    process.exit(2);
  }
  // `dm` is the operator's DM (the first WHATSAPP_ALLOWED_DM_JIDS entry).
  const target = args[0] === "dm" ? ([...config.allowedDmSenders][0] ?? "") : args[0];
  const message = args.slice(1).join(" ");
  // Only the operator's DM or a configured group, whoever the caller is. A
  // group JID that is not configured by JID may still be a configured group
  // by name: that is decided from the group list once connected.
  const early = target.endsWith("@g.us") ? null : enqueueRefusal(target, config);
  if (early) refuseTarget(target, early);
  // Direct sends are for the operator's own terminal only (ADR-033).
  refuseUnlessAllowed("send", "send", config.dbPath);

  const authDir = path.join(workspaceRoot, "messaging/whatsapp/auth");
  const { state, saveCreds } = await useMultiFileAuthState(authDir);
  const { version } = await fetchLatestWaWebVersion({});

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

  await new Promise<void>((resolve, reject) => {
    sock.ev.on("connection.update", async (update) => {
      const { connection } = update;
      if (connection === "open") {
        try {
          const groups = isOperatorDm(target, config)
            ? new GroupAllowlist(config)
            : new GroupAllowlist(
                config,
                Object.entries(await sock.groupFetchAllParticipating()).map(([jid, meta]) => ({
                  jid,
                  subject: meta?.subject ?? "",
                })),
              );
          const jid = resolveTarget(target, config, groups);
          if (!jid) {
            sock.end(undefined);
            refuseTarget(target, "no configured group has this JID or name");
          }

          log.info({ jid, len: message.length }, "send: dispatching message");
          const sent = await sock.sendMessage(jid, { text: message });
          log.info({ id: sent?.key.id, jid }, "send: ok");
          // Give the server a moment to flush before we close.
          await new Promise((r) => setTimeout(r, 1500));
          // end() closes the socket without invalidating the linked device —
          // logout() would unlink and force re-pairing on the next run.
          sock.end(undefined);
          resolve();
        } catch (e) {
          reject(e);
        }
      }
      // Other connection states (close, connecting) we ignore — we initiate
      // close ourselves after the send completes; nothing else to handle here.
    });
  });
}

main()
  .then(() => process.exit(0))
  .catch((e) => {
    log.fatal({ err: e?.message ?? e }, "send: failed");
    process.exit(1);
  });
