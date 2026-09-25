#!/usr/bin/env node
// One-time cleanup: remove Signal key material from WhatsApp bot logs written
// before the runtime console filter existed (src/key_redaction.ts, ADR-027
// amendment 2026-09).
//
// libsignal logged `console.info("Closing session:", session)` (and
// "Opening session:", "Removing old closed session:", "Session already
// closed"). Node printed the SessionEntry across many lines, with the ratchet
// private key, root key, chain ids and other keys as `<Buffer …>` values.
// This script finds each such block (header line through the closing "}" at
// column 0) and replaces every byte value and every base64 key string inside
// it with [redacted]. The block's other fields (registrationId, counters,
// timestamps, baseKeyType) and every line outside the blocks are kept as
// they are. Outside the blocks it also redacts key-named fields
// (`privKey: <Buffer …>`, `"privKey":"…"`), if any exist.
//
// Usage (from the workspace root):
//   node messaging/whatsapp/scripts/redact-signal-logs.mjs --dry-run
//   node messaging/whatsapp/scripts/redact-signal-logs.mjs
//   node messaging/whatsapp/scripts/redact-signal-logs.mjs [--dry-run] <file>...
//
// With no files: $NUCLEUS_WORKSPACE_ROOT/memory/whatsapp.log and
// whatsapp.log.<n> (the workspace root defaults to three levels above this
// script). --dry-run reports the counts and writes nothing.
//
// Files are rewritten in place (same inode): the running bot keeps its file
// descriptor, and no copy with the keys is left behind. A file that grows
// while it is processed is read again, up to 5 times. Lines the bot appends
// in the moment between the last read and the write can be lost, so run it
// while the bot is stopped when possible. Running it twice changes nothing
// the second time.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";

export const MARK = "[redacted]";

const HEADER = /^(Closing session:|Opening session:|Removing old closed session:|Session already closed:?) SessionEntry \{/;
const BUFFER = /<Buffer(?: [0-9a-f]{2})*(?: \.\.\. \d+ more bytes?)?>/g;
// A quoted base64 string of at least 16 characters (a key or a chain id).
const B64_QUOTED = /(['"])[A-Za-z0-9+/]{16,}={0,2}\1/g;
// The same as an unquoted object key: Node prints a key without quotes when
// it is a valid identifier (a base64 chain id with no "+" or "/"). A chain id
// is 44 characters; field names are shorter than 40.
const B64_BARE_KEY = /^(\s*)[A-Za-z_$][A-Za-z0-9]{39,}(?=: )/;
const KEY_NAMES =
  "privKey|private|privateKey|rootKey|chainKey|messageKeys|ephemeralKeyPair|lastRemoteEphemeralKey|baseKey|remoteIdentityKey|noiseKey|signedIdentityKey|signedPreKey|pairingEphemeralKeyPair|advSecretKey|keyData|mediaKey|macKey|cipherKey|secretKey";
const INSPECT_KEY_FIELD = new RegExp(`\\b(${KEY_NAMES})(\\s*:\\s*)<Buffer[^>]*>`, "g");
const JSON_KEY_FIELD = new RegExp(`"(${KEY_NAMES})"(\\s*:\\s*)"[^"]*"`, "g");

/** Redact one line inside a session block. */
function redactBlockLine(line, stats) {
  let out = line.replace(BUFFER, () => {
    stats.values += 1;
    return MARK;
  });
  out = out.replace(B64_QUOTED, (_m, q) => {
    stats.values += 1;
    return `${q}${MARK}${q}`;
  });
  out = out.replace(B64_BARE_KEY, (_m, indent) => {
    stats.values += 1;
    return `${indent}${MARK}`;
  });
  return out;
}

/** Redact key-named fields on a line outside the blocks. */
function redactOtherLine(line, stats) {
  let out = line.replace(INSPECT_KEY_FIELD, (_m, name, sep) => {
    stats.values += 1;
    return `${name}${sep}${MARK}`;
  });
  out = out.replace(JSON_KEY_FIELD, (_m, name, sep) => {
    stats.values += 1;
    return `"${name}"${sep}"${MARK}"`;
  });
  return out;
}

/** Redact a whole log text. Returns the new text and the counts. Pure. */
export function redactText(text) {
  const stats = { blocks: 0, truncatedBlocks: 0, values: 0, linesChanged: 0, lines: 0 };
  const lines = text.split("\n");
  stats.lines = text.endsWith("\n") ? lines.length - 1 : lines.length;
  const out = new Array(lines.length);
  let inBlock = false;
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    let next;
    if (!inBlock && HEADER.test(line)) {
      stats.blocks += 1;
      next = redactBlockLine(line, stats);
      // A block printed on one line ends on the same line.
      inBlock = !/\}\s*$/.test(line);
    } else if (inBlock) {
      next = redactBlockLine(line, stats);
      if (line === "}") inBlock = false;
    } else {
      next = redactOtherLine(line, stats);
    }
    if (next !== line) stats.linesChanged += 1;
    out[i] = next;
  }
  if (inBlock) stats.truncatedBlocks += 1;
  return { text: out.join("\n"), stats };
}

/** The default files: memory/whatsapp.log and memory/whatsapp.log.<n>. */
export function defaultFiles(workspaceRoot) {
  const dir = path.join(workspaceRoot, "memory");
  let names = [];
  try {
    names = fs.readdirSync(dir);
  } catch {
    return [];
  }
  return names
    .filter((n) => /^whatsapp\.log(\.\d+)?$/.test(n))
    .sort()
    .map((n) => path.join(dir, n));
}

/** Process one file. Writes in place (same inode) unless dryRun. */
export function processFile(file, { dryRun }) {
  for (let round = 0; round < 5; round++) {
    const before = fs.readFileSync(file);
    const text = before.toString("utf8");
    const { text: redacted, stats } = redactText(text);
    if (dryRun || redacted === text) return { file, written: false, ...stats };
    const fd = fs.openSync(file, "r+");
    try {
      // Grown since the read: process the whole file again.
      if (fs.fstatSync(fd).size !== before.length) continue;
      const buf = Buffer.from(redacted, "utf8");
      fs.writeSync(fd, buf, 0, buf.length, 0);
      fs.ftruncateSync(fd, buf.length);
      fs.fsyncSync(fd);
    } finally {
      fs.closeSync(fd);
    }
    return { file, written: true, ...stats };
  }
  throw new Error(`${file}: kept growing while it was processed; stop the bot and run again`);
}

function main(argv) {
  const dryRun = argv.includes("--dry-run");
  const unknown = argv.filter((a) => a.startsWith("--") && a !== "--dry-run");
  if (unknown.length) {
    console.error(`unknown option(s): ${unknown.join(" ")}`);
    process.exit(2);
  }
  const workspaceRoot =
    process.env.NUCLEUS_WORKSPACE_ROOT ?? path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
  const explicit = argv.filter((a) => !a.startsWith("--"));
  const files = explicit.length ? explicit : defaultFiles(workspaceRoot);
  if (files.length === 0) {
    console.error("no log files found");
    process.exit(1);
  }
  const totals = { blocks: 0, truncatedBlocks: 0, values: 0, linesChanged: 0, lines: 0 };
  for (const f of files) {
    const r = processFile(f, { dryRun });
    for (const k of Object.keys(totals)) totals[k] += r[k];
    console.log(
      `${dryRun ? "[dry-run] " : ""}${f}: ${r.blocks} session blocks, ${r.values} key values, ${r.linesChanged} of ${r.lines} lines changed${r.truncatedBlocks ? `, ${r.truncatedBlocks} block(s) cut off at end of file` : ""}${r.written ? " — rewritten" : dryRun ? "" : " — unchanged"}`,
    );
  }
  console.log(
    `${dryRun ? "[dry-run] " : ""}total: ${files.length} files, ${totals.blocks} session blocks, ${totals.values} key values, ${totals.linesChanged} lines changed`,
  );
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  main(process.argv.slice(2));
}
