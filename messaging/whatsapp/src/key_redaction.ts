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

/** How deep the redactor walks. Deeper objects are replaced, not printed. */
export const REDACT_MAX_DEPTH = 8;
export const DEPTH_REDACTED = "[redacted: nested too deep]";

/** Values printed as they are: they cannot hold key-named fields. Raw bytes
 *  are key material only under a key-named field, which is replaced. */
function isLeafObject(v: object): boolean {
  return ArrayBuffer.isView(v) || v instanceof ArrayBuffer || v instanceof Date || v instanceof RegExp;
}

/** Own enumerable entries of any other object: plain objects, arrays, class
 *  instances and Errors alike (fail closed: an unknown class may hold keys). */
function entriesOf(v: object): Array<[string, unknown]> {
  if (v instanceof Map) return [...v.entries()].map(([k, x]) => [String(k), x]);
  if (v instanceof Set) return [...v.values()].map((x, i) => [String(i), x]);
  const entries = Object.entries(v);
  // An Error's `cause` is not enumerable, and the console prints it.
  if (v instanceof Error && Object.prototype.hasOwnProperty.call(v, "cause") && !entries.some(([k]) => k === "cause")) {
    entries.push(["cause", (v as { cause?: unknown }).cause]);
  }
  return entries;
}

/** Whether `v` holds key material, or may hold it beyond REDACT_MAX_DEPTH
 *  (then true: fail closed). */
export function hasKeyMaterial(v: unknown, depth = REDACT_MAX_DEPTH, seen = new WeakSet<object>()): boolean {
  if (v === null || typeof v !== "object") return false;
  if (seen.has(v)) return false; // its first visit already answered for it
  seen.add(v);
  if (isSessionEntry(v)) return true;
  if (isLeafObject(v)) return false;
  const entries = entriesOf(v);
  if (depth <= 0) return entries.length > 0;
  for (const [k, child] of entries) {
    if (KEY_FIELD.test(k)) return true;
    if (hasKeyMaterial(child, depth - 1, seen)) return true;
  }
  return false;
}

/** A redacted copy of `v`: the values of key-named fields replaced, every
 *  SessionEntry replaced by a marker, and objects nested deeper than
 *  REDACT_MAX_DEPTH replaced. A value with no key material is returned
 *  unchanged (same reference). Otherwise every object in the graph is copied
 *  once: a shared reference or a cycle reuses the same redacted copy, so no
 *  path through the result reaches an original object that holds keys.
 *  Errors keep their message and stack; their own fields are redacted. */
export function redactKeyMaterial<T>(v: T): T {
  if (!hasKeyMaterial(v)) return v;
  return redactInto(v, REDACT_MAX_DEPTH, new WeakMap()) as T;
}

function redactInto(v: unknown, depth: number, memo: WeakMap<object, unknown>): unknown {
  if (v === null || typeof v !== "object") return v;
  const known = memo.get(v);
  if (known !== undefined) return known;
  if (isSessionEntry(v)) {
    memo.set(v, SESSION_REDACTED);
    return SESSION_REDACTED;
  }
  if (isLeafObject(v)) return v;
  if (depth <= 0) {
    memo.set(v, DEPTH_REDACTED);
    return DEPTH_REDACTED;
  }
  let out: Record<string, unknown> | unknown[];
  if (Array.isArray(v)) {
    out = [];
  } else if (v instanceof Error) {
    const e = new Error(v.message);
    e.name = v.name;
    if (v.stack) e.stack = v.stack;
    out = e as unknown as Record<string, unknown>;
  } else {
    out = {};
  }
  // Registered before the children, so a cycle back to `v` gets this copy.
  memo.set(v, out);
  for (const [k, child] of entriesOf(v)) {
    (out as Record<string, unknown>)[k] = KEY_FIELD.test(k) ? REDACTED : redactInto(child, depth - 1, memo);
  }
  return out;
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
