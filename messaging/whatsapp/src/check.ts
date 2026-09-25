// One-shot: check which of N candidate phone numbers exist on WhatsApp.
import {
  default as makeWASocket,
  useMultiFileAuthState,
  makeCacheableSignalKeyStore,
  Browsers,
} from "@whiskeysockets/baileys";
import pino from "pino";
import { installConsoleKeyFilter } from "./key_redaction.js";
import { resolveWaVersion, waVersionCachePath } from "./wa_version.js";
import path from "node:path";

const log = pino({ level: "info" });
const baileysLogger = pino({ level: "silent" });
// libsignal prints Signal session state (private keys) through the console.
installConsoleKeyFilter();

async function main() {
  const candidates = process.argv.slice(2);
  if (!candidates.length) {
    console.error("usage: tsx src/check.ts <num1> <num2> ...");
    process.exit(2);
  }

  const workspaceRoot =
    process.env.NUCLEUS_WORKSPACE_ROOT ??
    path.resolve(import.meta.dirname, "..", "..", "..");
  const authDir = path.join(workspaceRoot, "messaging/whatsapp/auth");
  const { state, saveCreds } = await useMultiFileAuthState(authDir);
  const { version } = await resolveWaVersion({ cachePath: waVersionCachePath(workspaceRoot), log });

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

  await new Promise<void>((resolve) => {
    sock.ev.on("connection.update", async (update) => {
      if (update.connection === "open") {
        for (const c of candidates) {
          const digits = c.replace(/\D/g, "");
          try {
            const res = await sock.onWhatsApp(digits);
            const exists = res && res[0]?.exists;
            log.info({ candidate: c, digits, exists, jid: exists ? res[0].jid : null });
          } catch (e) {
            log.warn({ candidate: c, digits, err: (e as Error).message });
          }
        }
        sock.end(undefined);
        resolve();
      }
    });
  });
}

main()
  .then(() => process.exit(0))
  .catch((e) => {
    log.fatal({ err: e?.message }, "check: failed");
    process.exit(1);
  });
