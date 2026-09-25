// Key material stays out of the logs (ADR-027 amendment, 2026-09). The
// sessions here are synthetic: random bytes built at runtime.

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { Console } from "node:console";
import { Writable } from "node:stream";
import { createRequire } from "node:module";
import { randomBytes } from "node:crypto";
import {
  SESSION_REDACTED,
  installConsoleKeyFilter,
  makeBaileysLogger,
  redactKeyMaterial,
  sanitizeConsoleArgs,
} from "./key_redaction.js";

const require = createRequire(import.meta.url);
// libsignal is Baileys' dependency; the session class is what it logs.
const SessionRecord = require("libsignal/src/session_record.js");

function syntheticSession() {
  const b = (n: number) => randomBytes(n).toString("base64");
  const data = {
    registrationId: 4242,
    currentRatchet: {
      ephemeralKeyPair: { pubKey: b(33), privKey: b(32) },
      lastRemoteEphemeralKey: b(33),
      previousCounter: 0,
      rootKey: b(32),
    },
    indexInfo: { baseKey: b(33), baseKeyType: 1, closed: -1, used: 1, created: 1, remoteIdentityKey: b(33) },
    _chains: { [b(33)]: { chainKey: { counter: 3, key: b(32) }, chainType: 1, messageKeys: { "1": b(32) } } },
    pendingPreKey: { signedKeyId: 5, baseKey: b(33), preKeyId: 7 },
  };
  const entry = SessionRecord.createEntry().constructor.deserialize(data);
  return { entry, secrets: [data.currentRatchet.ephemeralKeyPair.privKey, data.currentRatchet.rootKey] };
}

/** Hex as Node prints a Buffer: "40 b6 ce …". */
const hexOf = (b64: string) => [...Buffer.from(b64, "base64")].map((x) => x.toString(16).padStart(2, "0")).join(" ");

function capture(): { out: () => string; con: Console } {
  let text = "";
  const stream = new Writable({
    write(chunk, _enc, cb) {
      text += chunk.toString();
      cb();
    },
  });
  return { out: () => text, con: new Console({ stdout: stream, stderr: stream }) };
}

test("libsignal session lines keep their text and lose the session object", () => {
  const { entry, secrets } = syntheticSession();
  const c = capture();
  installConsoleKeyFilter(c.con);
  c.con.info("Closing session:", entry);
  c.con.warn("Session already closed", entry);
  const out = c.out();
  assert.match(out, /^Closing session: \[signal session state redacted\]$/m);
  assert.match(out, /^Session already closed \[signal session state redacted\]$/m);
  for (const s of secrets) assert.ok(!out.includes(hexOf(s)), "no private key bytes");
  assert.doesNotMatch(out, /privKey|rootKey/);
});

test("the real libsignal closeSession path writes no key material through the global console", () => {
  const { entry, secrets } = syntheticSession();
  installConsoleKeyFilter();
  const original = process.stdout.write.bind(process.stdout);
  let out = "";
  (process.stdout as any).write = (chunk: any, ...rest: any[]) => {
    out += chunk.toString();
    return true;
  };
  try {
    const record = new SessionRecord();
    record.setSession(entry);
    record.closeSession(entry);
    record.openSession(entry);
  } finally {
    (process.stdout as any).write = original;
  }
  assert.match(out, /Closing session: \[signal session state redacted\]/);
  assert.match(out, /Opening session: \[signal session state redacted\]/);
  for (const s of secrets) assert.ok(!out.includes(hexOf(s)));
});

test("other console output is unchanged unless it carries key-named fields", () => {
  const plain = { chatId: "x", n: 1, key: { id: "MSGID", remoteJid: "r" } };
  assert.equal(sanitizeConsoleArgs(["hello", plain])[1], plain, "same reference: nothing to redact");
  const err = new Error("boom");
  assert.equal(sanitizeConsoleArgs([err])[0], err);
  const withKeys = { creds: { noiseKey: { private: "AAA", public: "BBB" }, registrationId: 1 } };
  const red = sanitizeConsoleArgs(["creds", withKeys])[1] as any;
  assert.equal(red.creds.noiseKey, "[redacted]");
  assert.equal(red.creds.registrationId, 1);
  assert.equal(withKeys.creds.noiseKey.private, "AAA", "the original object is not modified");
});

test("redactKeyMaterial replaces a SessionEntry nested in a log object", () => {
  const { entry } = syntheticSession();
  const red = redactKeyMaterial({ jid: "j", session: entry }) as any;
  assert.equal(red.session, SESSION_REDACTED);
  assert.equal(red.jid, "j");
});

test("the Baileys logger writes to its file with key material redacted", () => {
  const file = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-blog-")), "whatsapp-baileys.log");
  const logger = makeBaileysLogger(file, "info");
  logger.child({ class: "baileys" }).info({ msgAttrs: { id: "M1" }, retryCount: 1 }, "sent retry receipt");
  logger.info({ creds: { signedIdentityKey: { private: "SECRETVALUE" } } }, "creds");
  logger.debug({ x: 1 }, "below the level");
  const lines = fs.readFileSync(file, "utf8").trim().split("\n").map((l) => JSON.parse(l));
  assert.equal(lines.length, 2);
  assert.equal(lines[0].msg, "sent retry receipt");
  assert.equal(lines[0].retryCount, 1);
  assert.equal(lines[1].creds.signedIdentityKey, "[redacted]");
  assert.ok(!fs.readFileSync(file, "utf8").includes("SECRETVALUE"));
});
