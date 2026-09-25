// Keep Signal key material out of the logs (ADR-027 amendment, 2026-09).
//
// libsignal (the Signal implementation Baileys uses) writes session state
// to the console: `console.info("Closing session:", session)`, and the same
// for "Opening session:", "Removing old closed session:" and "Session already
// closed". Node prints the whole SessionEntry, including the ratchet private
// key, the root key and the chain keys. The bot's stdout is its log file
// (memory/whatsapp.log), so every session change wrote keys into the log.
//
// `installConsoleKeyFilter` wraps the console methods once, at process
// start: a libsignal session line keeps its text and loses the object, and
// any other logged object loses the values of key-named fields. The Baileys
// pino logger passes every record through `redactKeyMaterial` as well.
//
// messaging/whatsapp/scripts/redact-signal-logs.mjs removes the same
// material from log files written before this filter existed.

import pino, { type Logger } from "pino";

/** Field names whose values are key material (libsignal session state,
 *  Baileys auth creds, media keys). A message key (`key: {remoteJid, id}`)
 *  is not listed: it identifies a message and is not secret. */
export const KEY_FIELD =
  /^(privKey|private|privateKey|rootKey|chainKey|messageKeys|ephemeralKeyPair|lastRemoteEphemeralKey|baseKey|remoteIdentityKey|pendingPreKey|noiseKey|signedIdentityKey|signedPreKey|pairingEphemeralKeyPair|advSecretKey|keyData|mediaKey|macKey|cipherKey|appStateSyncKey|_chains|currentRatchet|secret|secretKey)$/i;

/** libsignal's console lines that carry a whole session object. */
export const SESSION_LOG_LINE = /^(Closing session:|Opening session:|Removing old closed session:|Session already closed)/;

export const REDACTED = "[redacted]";
export const SESSION_REDACTED = "[signal session state redacted]";

/** A libsignal SessionEntry (or its serialized form). */
function isSessionEntry(v: object): boolean {
  const o = v as Record<string, unknown>;
  return (
    v.constructor?.name === "SessionEntry" ||
    ("currentRatchet" in o && "indexInfo" in o) ||
    ("_chains" in o && "indexInfo" in o)
  );
}

function isPlainContainer(v: object): boolean {
  if (Array.isArray(v)) return true;
  const proto = Object.getPrototypeOf(v);
  return proto === Object.prototype || proto === null || isSessionEntry(v);
}

/** Whether `v` holds key material within `depth` levels. */
export function hasKeyMaterial(v: unknown, depth = 6, seen = new WeakSet<object>()): boolean {
  if (v === null || typeof v !== "object" || depth < 0) return false;
  if (seen.has(v)) return false;
  seen.add(v);
  if (isSessionEntry(v)) return true;
  if (!isPlainContainer(v)) return false;
  for (const [k, child] of Object.entries(v)) {
    if (KEY_FIELD.test(k)) return true;
    if (hasKeyMaterial(child, depth - 1, seen)) return true;
  }
  return false;
}

/** A copy of `v` with the values of key-named fields replaced, and every
 *  SessionEntry replaced by a marker. Values without key material are
 *  returned unchanged (same reference); Errors, Buffers and class
 *  instances other than SessionEntry are not walked. */
export function redactKeyMaterial<T>(v: T, depth = 6, seen = new WeakSet<object>()): T {
  if (v === null || typeof v !== "object" || depth < 0) return v;
  if (seen.has(v as object)) return v;
  seen.add(v as object);
  if (isSessionEntry(v as object)) return SESSION_REDACTED as unknown as T;
  if (!isPlainContainer(v as object)) return v;
  if (!hasKeyMaterial(v, depth)) return v;
  if (Array.isArray(v)) return v.map((c) => redactKeyMaterial(c, depth - 1, seen)) as unknown as T;
  const out: Record<string, unknown> = {};
  for (const [k, child] of Object.entries(v as Record<string, unknown>)) {
    out[k] = KEY_FIELD.test(k) ? REDACTED : redactKeyMaterial(child, depth - 1, seen);
  }
  return out as T;
}

/** The console arguments with key material removed. A libsignal session
 *  line keeps its text; its object arguments become a marker. */
export function sanitizeConsoleArgs(args: unknown[]): unknown[] {
  if (typeof args[0] === "string" && SESSION_LOG_LINE.test(args[0])) {
    return args.map((a, i) => (i > 0 && a !== null && typeof a === "object" ? SESSION_REDACTED : a));
  }
  return args.map((a) => redactKeyMaterial(a));
}

const METHODS = ["log", "info", "warn", "error", "debug", "trace"] as const;
let installed = false;

/** Wrap the console methods so nothing written through them carries key
 *  material. Idempotent. Call before the first Baileys socket is made. */
export function installConsoleKeyFilter(target: Console = console): void {
  if (target === console) {
    if (installed) return;
    installed = true;
  }
  for (const m of METHODS) {
    const original = target[m].bind(target) as (...a: unknown[]) => void;
    (target as unknown as Record<string, unknown>)[m] = (...args: unknown[]) => original(...sanitizeConsoleArgs(args));
  }
}

/** The Baileys logger: level `NUCLEUS_BAILEYS_LOG` (default info), written
 *  synchronously to `file` (memory/whatsapp-baileys.log for the bot), every
 *  record passed through `redactKeyMaterial`. `file` null = silent. */
export function makeBaileysLogger(file: string | null, level = process.env.NUCLEUS_BAILEYS_LOG ?? "info"): Logger {
  if (!file || level === "silent") return pino({ level: "silent" });
  return pino(
    {
      level,
      base: { src: "baileys" },
      formatters: { log: (obj) => redactKeyMaterial(obj) },
    },
    pino.destination({ dest: file, sync: true, mkdir: true }),
  );
}
