// One-shot group lister. Reuses the paired auth state, opens a connection,
// dumps the user's currently-participating groups, and exits cleanly.
//
// Usage: npm run list-groups
//
// IMPORTANT: stop the running WhatsApp service first (launchctl stop dev.nucleus.whatsapp
// or similar). Two concurrent Baileys connections from the same auth dir
// will race each other.

import {
  default as makeWASocket,
  useMultiFileAuthState,
  fetchLatestWaWebVersion,
  makeCacheableSignalKeyStore,
  Browsers,
} from "@whiskeysockets/baileys";
import pino from "pino";
import path from "node:path";

const log = pino({ level: process.env.NUCLEUS_LOG ?? "info" });
const baileysLogger = pino({ level: "silent" });

async function main() {
  const workspaceRoot =
    process.env.NUCLEUS_WORKSPACE_ROOT ??
    path.resolve(import.meta.dirname, "..", "..", "..");

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
      if (update.connection === "open") {
        try {
          const groups = await sock.groupFetchAllParticipating();
          const rows = Object.entries(groups)
            .map(([jid, meta]: [string, any]) => ({
              jid,
              name: (meta?.subject ?? "").trim(),
              participants: (meta?.participants ?? []).length,
            }))
            .sort((a, b) => a.name.localeCompare(b.name));

          // Print as a clean table for human eyeballs + JSON for parsing.
          console.log("\nParticipating groups:");
          console.log("====================");
          for (const r of rows) {
            console.log(`  ${r.name.padEnd(40)} ${String(r.participants).padStart(3)}p  ${r.jid}`);
          }
          console.log("");
          console.log(JSON.stringify({ groups: rows }, null, 2));

          await new Promise((r) => setTimeout(r, 1000));
          sock.end(undefined);
          resolve();
        } catch (e) {
          reject(e);
        }
      }
    });
  });
}

main()
  .then(() => process.exit(0))
  .catch((e) => {
    log.fatal({ err: e?.message ?? e }, "list-groups: failed");
    process.exit(1);
  });
