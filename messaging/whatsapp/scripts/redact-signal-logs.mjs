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
//   node messaging/whatsapp/scripts/redact-signal-logs.mjs [--dry-run] [--force] <file>...
//
// With no files: $NUCLEUS_WORKSPACE_ROOT/memory/whatsapp.log and
// whatsapp.log.<n> (the workspace root defaults to three levels above this
// script). --dry-run reports the counts and writes nothing.
//
// Run it only while the WhatsApp bot is stopped. The lsof check refuses a
// file that another process has open for writing, but that check and the
// final rename are separate steps: a writer that opens the file between
// them can lose its line or keep writing into the replaced inode.
//
// A file that any process has open for writing (lsof; the running bot's
// stdout is one of these files) is refused: the script exits 3, names the
// file and the pid, and writes nothing. Stop the bot first. --force replaces
// the file anyway; the writing process then writes to the old, deleted file
// until it reopens its log. If lsof cannot run, every file counts as open.
//
// Each file is written to a temp file in the same directory, with the same
// mode, and renamed over the original, so no partial file ever exists. The
// rename happens only if the file's size and modification time did not
// change while it was processed; otherwise the file is left as it was and
// the script exits 1. The old file content is not kept anywhere. Running the
// script twice changes nothing the second time.

import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";
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


/** Processes that have `file` open for writing, from `lsof -F pan`
 *  (macOS/BSD field output: p<pid>, c<command>, a<access r|w|u>).
 *  Throws when lsof cannot run: the caller refuses unless --force. */
export function writersOf(file, lsof = "lsof") {
  const res = spawnSync(lsof, ["-F", "pca", "--", file], { encoding: "utf8" });
  if (res.error) throw new Error(`cannot run ${lsof}: ${res.error.message}`);
  // lsof exits 1 when no process has the file open.
  if (res.status !== 0 && res.status !== 1) throw new Error(`${lsof} failed (exit ${res.status}): ${res.stderr.trim()}`);
  const writers = [];
  let pid = null;
  let command = "";
  for (const line of res.stdout.split("\n")) {
    const tag = line[0];
    const val = line.slice(1);
    if (tag === "p") {
      pid = Number(val);
      command = "";
    } else if (tag === "c") {
      command = val;
    } else if (tag === "a" && (val === "w" || val === "u") && pid !== null) {
      if (!writers.some((w) => w.pid === pid)) writers.push({ pid, command });
    }
  }
  return writers;
}

/** Process one file. Unless dryRun, writes the redacted text to a temp file
 *  in the same directory with the same mode and renames it over the file,
 *  only when the file's size and mtime did not change while it was
 *  processed (a file that changed is left as it was, and an error is
 *  thrown). */
export function processFile(file, { dryRun, beforeRename }) {
  const before = fs.statSync(file);
  const text = fs.readFileSync(file, "utf8");
  const { text: redacted, stats } = redactText(text);
  if (dryRun || redacted === text) return { file, written: false, ...stats };
  const tmp = path.join(path.dirname(file), `.${path.basename(file)}.redact-${process.pid}.tmp`);
  try {
    fs.writeFileSync(tmp, redacted, { mode: before.mode & 0o7777, flag: "wx" });
    fs.chmodSync(tmp, before.mode & 0o7777);
    const fd = fs.openSync(tmp, "r");
    try {
      fs.fsyncSync(fd);
    } finally {
      fs.closeSync(fd);
    }
    beforeRename?.(); // test hook: simulate a write during processing
    const after = fs.statSync(file);
    if (after.size !== before.size || after.mtimeMs !== before.mtimeMs || after.ino !== before.ino) {
      throw new Error(`${file} changed while it was processed; nothing written — stop the writer and run again`);
    }
    fs.renameSync(tmp, file);
  } finally {
    fs.rmSync(tmp, { force: true });
  }
  return { file, written: true, ...stats };
}

function main(argv) {
  const known = new Set(["--dry-run", "--force"]);
  const dryRun = argv.includes("--dry-run");
  const force = argv.includes("--force");
  const unknown = argv.filter((a) => a.startsWith("--") && !known.has(a));
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

  // Refuse before writing anything when a process has a file open for
  // writing: replacing the file would send that process's later lines to
  // the old, deleted file.
  const busy = [];
  for (const f of files) {
    let writers;
    try {
      writers = writersOf(f, process.env.NUCLEUS_LSOF ?? "lsof");
    } catch (e) {
      busy.push({ file: f, detail: `${e.message} (cannot check for writers)` });
      continue;
    }
    if (writers.length) {
      busy.push({ file: f, detail: `open for writing by ${writers.map((w) => `pid ${w.pid}${w.command ? ` (${w.command})` : ""}`).join(", ")}` });
    }
  }
  for (const b of busy) {
    const prefix = dryRun ? "[dry-run] warning: " : force ? "warning (--force): " : "refused: ";
    console.error(`${prefix}${b.file} is ${b.detail}`);
  }
  if (busy.length && !dryRun && !force) {
    console.error(
      "Nothing was written. Stop the process that writes the file (for the bot: its launchd service) and run again, or pass --force to replace the file anyway (that process then writes to the old, deleted file until it reopens it).",
    );
    process.exit(3);
  }

  const totals = { blocks: 0, truncatedBlocks: 0, values: 0, linesChanged: 0, lines: 0 };
  let failed = 0;
  for (const f of files) {
    let r;
    try {
      r = processFile(f, { dryRun });
    } catch (e) {
      failed += 1;
      console.error(`error: ${e.message}`);
      continue;
    }
    for (const k of Object.keys(totals)) totals[k] += r[k];
    console.log(
      `${dryRun ? "[dry-run] " : ""}${f}: ${r.blocks} session blocks, ${r.values} key values, ${r.linesChanged} of ${r.lines} lines changed${r.truncatedBlocks ? `, ${r.truncatedBlocks} block(s) cut off at end of file` : ""}${r.written ? " — rewritten" : dryRun ? "" : " — unchanged"}`,
    );
  }
  console.log(
    `${dryRun ? "[dry-run] " : ""}total: ${files.length} files, ${totals.blocks} session blocks, ${totals.values} key values, ${totals.linesChanged} lines changed${failed ? `, ${failed} failed` : ""}`,
  );
  if (failed) process.exit(1);
}

if (process.argv[1] && import.meta.url === pathToFileURL(path.resolve(process.argv[1])).href) {
  main(process.argv.slice(2));
}
