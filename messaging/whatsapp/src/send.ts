// One-shot send. Reuses the paired auth state, opens a connection, sends a
// single message, and exits cleanly.
//
// Usage: npm run send -- <phone-or-jid> <message...>
//   phone: international format, digits only (country code + number, no '+')
//   jid:   already-formed like <id>@s.whatsapp.net or <id>@g.us
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

const log = pino({ level: process.env.NUCLEUS_LOG ?? "info" });
const baileysLogger = pino({ level: "silent" });

async function main() {
  const args = process.argv.slice(2);
  if (args.length < 2) {
    console.error("usage: npm run send -- <phone-or-jid> <message...>");
    process.exit(2);
  }
  const target = args[0];
  const message = args.slice(1).join(" ");

  const workspaceRoot =
    process.env.NUCLEUS_WORKSPACE_ROOT ??
    path.resolve(import.meta.dirname, "..", "..", "..");
  const config = loadConfig(workspaceRoot, false);

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
          // Resolve target → JID. If it already looks like a JID, pass through.
          let jid: string;
          if (target.includes("@")) {
            jid = target;
          } else {
            const digits = target.replace(/\D/g, "");
            const res = await sock.onWhatsApp(digits);
            if (!res || !res[0]?.exists) {
              throw new Error(`${digits} is not on WhatsApp (or check returned empty)`);
            }
            jid = res[0].jid;
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
