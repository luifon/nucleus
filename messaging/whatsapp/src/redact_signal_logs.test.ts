// scripts/redact-signal-logs.mjs on synthetic log files (never the real
// logs). The session blocks are printed exactly as the console printed them:
// util.format of a real libsignal SessionEntry built from random bytes.

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import util from "node:util";
import { spawn, spawnSync } from "node:child_process";
import { createRequire } from "node:module";
import { randomBytes } from "node:crypto";
import { pathToFileURL } from "node:url";

const require = createRequire(import.meta.url);
const SessionRecord = require("libsignal/src/session_record.js");
const SCRIPT = path.resolve(import.meta.dirname, "..", "scripts", "redact-signal-logs.mjs");
const mod: any = await import(pathToFileURL(SCRIPT).href);

function sessionBlock(header: string) {
  const b = (n: number) => randomBytes(n).toString("base64");
  const data = {
    registrationId: 4242,
    currentRatchet: {
      ephemeralKeyPair: { pubKey: b(33), privKey: b(32) },
      lastRemoteEphemeralKey: b(33),
      previousCounter: 0,
      rootKey: b(32),
    },
    indexInfo: { baseKey: b(33), baseKeyType: 1, closed: -1, used: 1700000000000, created: 1700000000000, remoteIdentityKey: b(33) },
    _chains: { [b(33)]: { chainKey: { counter: 3, key: b(32) }, chainType: 1, messageKeys: {} } },
    pendingPreKey: { signedKeyId: 5, baseKey: b(33), preKeyId: 7 },
  };
  const entry = SessionRecord.createEntry().constructor.deserialize(data);
  const hex = (s: string) => [...Buffer.from(s, "base64")].map((x) => x.toString(16).padStart(2, "0")).join(" ");
  return {
    text: util.format(header, entry),
    secrets: [
      hex(data.currentRatchet.ephemeralKeyPair.privKey),
      hex(data.currentRatchet.rootKey),
      Object.keys(data._chains)[0],
    ],
  };
}

const PINO_A = '{"level":30,"time":1790000000000,"pid":1,"msg":"whatsapp: connected"}';
const PINO_B = '{"level":40,"time":1790000000001,"pid":1,"reason":428,"msg":"whatsapp: connection closed"}';

function syntheticLog() {
  const closing = sessionBlock("Closing session:");
  const opening = sessionBlock("Opening session:");
  const already = sessionBlock("Session already closed");
  const text = [PINO_A, closing.text, "Session already open", opening.text, PINO_B, already.text, "plain line", ""].join("\n");
  return { text, secrets: [...closing.secrets, ...opening.secrets, ...already.secrets] };
}

test("redactText removes every key value from the session blocks and keeps everything else", () => {
  const { text, secrets } = syntheticLog();
  const { text: out, stats } = mod.redactText(text);
  for (const s of secrets) assert.ok(!out.includes(s), "no key bytes or chain id left");
  assert.doesNotMatch(out, /<Buffer/);
  assert.equal(stats.blocks, 3);
  assert.equal(stats.truncatedBlocks, 0);
  assert.ok(stats.values >= 3 * 8, `every buffer and chain id counted (${stats.values})`);
  // Same line count; every non-block line unchanged; block fields kept.
  const inLines = text.split("\n");
  const outLines = out.split("\n");
  assert.equal(outLines.length, inLines.length);
  for (const keep of [PINO_A, PINO_B, "Session already open", "plain line"]) assert.ok(outLines.includes(keep));
  assert.match(out, /^Closing session: SessionEntry \{$/m);
  assert.match(out, /registrationId: 4242,/);
  assert.match(out, /privKey: \[redacted\]/);
  assert.match(out, /rootKey: \[redacted\]/);
  assert.match(out, /baseKeyType: 1,/);
  // Idempotent.
  const again = mod.redactText(out);
  assert.equal(again.text, out);
  assert.equal(again.stats.values, 0);
  assert.equal(again.stats.linesChanged, 0);
});

test("a chain id printed as an unquoted key is redacted; field names are not", () => {
  const bare = "Ab" + "c1".repeat(21); // 44 characters, a valid identifier
  const block = [
    "Closing session: SessionEntry {",
    "  _chains: {",
    `    ${bare}: { chainKey: [Object], chainType: 1, messageKeys: {} }`,
    "  },",
    "  currentRatchet: {",
    "    lastRemoteEphemeralKey: <Buffer 0a 3f>,",
    "  },",
    "}",
  ].join("\n");
  const { text: out } = mod.redactText(block);
  assert.ok(!out.includes(bare));
  assert.match(out, /^    \[redacted\]: \{ chainKey/m);
  assert.match(out, /lastRemoteEphemeralKey: \[redacted\],/);
});

test("a block cut off at the end of the file is redacted to the end", () => {
  const { text, secrets } = sessionBlock("Closing session:");
  const cut = text.split("\n").slice(0, 10).join("\n");
  const { text: out, stats } = mod.redactText(cut);
  assert.equal(stats.truncatedBlocks, 1);
  for (const s of secrets) assert.ok(!out.includes(s));
});

test("key-named fields outside a block are redacted; other lines are not touched", () => {
  const line = '{"level":30,"creds":{"privKey":"QUJDREVGR0hJSktMTU5PUA=="},"msg":"x"}';
  const inspectLine = "  privKey: <Buffer 01 02 03>,";
  const { text: out } = mod.redactText([line, inspectLine, "keyboard: <Buffer 01>"].join("\n"));
  assert.equal(out.split("\n")[0], '{"level":30,"creds":{"privKey":"[redacted]"},"msg":"x"}');
  assert.equal(out.split("\n")[1], "  privKey: [redacted],");
  assert.equal(out.split("\n")[2], "keyboard: <Buffer 01>");
});

function workspace() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-redact-"));
  fs.mkdirSync(path.join(root, "memory"));
  const { text, secrets } = syntheticLog();
  const files = ["whatsapp.log", "whatsapp.log.0"].map((n) => path.join(root, "memory", n));
  for (const f of files) fs.writeFileSync(f, text, { mode: 0o640 });
  for (const f of files) fs.chmodSync(f, 0o640);
  fs.writeFileSync(path.join(root, "memory", "other.log"), text);
  return { root, text, secrets, files, env: { ...process.env, NUCLEUS_WORKSPACE_ROOT: root } };
}

/** Run the script; returns exit status and output instead of throwing. */
function run(args: string[], env: NodeJS.ProcessEnv) {
  const r = spawnSync(process.execPath, [SCRIPT, ...args], { env, encoding: "utf8" });
  return { status: r.status, stdout: r.stdout, stderr: r.stderr };
}

test("the CLI: dry run writes nothing; the real run replaces each file (temp + rename, mode kept) and is idempotent", () => {
  const w = workspace();
  const dry = run(["--dry-run"], w.env);
  assert.equal(dry.status, 0);
  assert.match(dry.stdout, /\[dry-run\] total: 2 files, 6 session blocks/);
  for (const f of w.files) assert.equal(fs.readFileSync(f, "utf8"), w.text, "dry run wrote nothing");

  const real = run([], w.env);
  assert.equal(real.status, 0, real.stderr);
  assert.match(real.stdout, /rewritten/);
  for (const f of w.files) {
    const out = fs.readFileSync(f, "utf8");
    for (const s of w.secrets) assert.ok(!out.includes(s));
    assert.ok(out.includes(PINO_B));
    assert.equal(fs.statSync(f).mode & 0o777, 0o640, "mode preserved");
  }
  assert.deepEqual(
    fs.readdirSync(path.join(w.root, "memory")).filter((n) => n.includes("redact")),
    [],
    "no temp file left behind",
  );
  assert.equal(fs.readFileSync(path.join(w.root, "memory", "other.log"), "utf8"), w.text, "only whatsapp.log*");

  const second = run([], w.env);
  assert.match(second.stdout, /total: 2 files, 6 session blocks, 0 key values, 0 lines changed/);
  assert.match(second.stdout, /— unchanged/);
});

test("a file open for writing by another process is refused (pid named, nothing written) unless --force", async () => {
  const w = workspace();
  // A child process holding whatsapp.log open for append, as the bot's stdout is.
  const holder = spawn(
    process.execPath,
    ["-e", `require("fs").openSync(${JSON.stringify(w.files[0])}, "a"); console.log("ready"); setInterval(() => {}, 1000);`],
    { stdio: ["ignore", "pipe", "inherit"] },
  );
  try {
    await new Promise<void>((resolve, reject) => {
      holder.stdout!.once("data", () => resolve());
      holder.once("error", reject);
    });
    const refused = run([], w.env);
    assert.equal(refused.status, 3);
    assert.match(refused.stderr, new RegExp(`refused: .*whatsapp\\.log is open for writing by pid ${holder.pid}`));
    assert.match(refused.stderr, /Nothing was written/);
    for (const f of w.files) assert.equal(fs.readFileSync(f, "utf8"), w.text, "no file was touched, not even the free one");

    const dry = run(["--dry-run"], w.env);
    assert.equal(dry.status, 0);
    assert.match(dry.stderr, new RegExp(`warning: .*pid ${holder.pid}`));

    const forced = run(["--force"], w.env);
    assert.equal(forced.status, 0, forced.stderr);
    for (const s of w.secrets) assert.ok(!fs.readFileSync(w.files[0], "utf8").includes(s));
  } finally {
    holder.kill();
  }
});

test("lsof unavailable: every file counts as open and the run is refused", () => {
  const w = workspace();
  const r = run([], { ...w.env, NUCLEUS_LSOF: "/nonexistent/lsof" });
  assert.equal(r.status, 3);
  assert.match(r.stderr, /cannot check for writers/);
  for (const f of w.files) assert.equal(fs.readFileSync(f, "utf8"), w.text);
});

test("a file that changes while it is processed is left as it was", () => {
  const w = workspace();
  assert.throws(
    () => mod.processFile(w.files[0], { dryRun: false, beforeRename: () => fs.appendFileSync(w.files[0], "late line\n") }),
    /changed while it was processed/,
  );
  assert.equal(fs.readFileSync(w.files[0], "utf8"), w.text + "late line\n", "the appended line survives");
  assert.deepEqual(fs.readdirSync(path.join(w.root, "memory")).filter((n) => n.includes("redact")), []);
});

test("writersOf parses lsof field output: only w/u access counts", () => {
  const w = workspace();
  // A fake lsof printing a reader and a writer.
  const fake = path.join(w.root, "fake-lsof.sh");
  fs.writeFileSync(fake, "#!/bin/sh\nprintf 'p100\\ncreader\\nf3\\nar\\np200\\ncwriter\\nf1\\naw\\np300\\ncboth\\nf4\\nau\\n'\n", { mode: 0o755 });
  assert.deepEqual(mod.writersOf(w.files[0], fake), [
    { pid: 200, command: "writer" },
    { pid: 300, command: "both" },
  ]);
});

