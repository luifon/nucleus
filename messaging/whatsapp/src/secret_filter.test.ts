import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { buildRules, filterOutbound, looksRandom, redactCredentials } from "./secret_filter.js";

const env = [
  "# comment",
  "DISCORD_BOT_TOKEN=\"tok-abcdefghijklmnopqrstuvwxyz\"",
  "WHATSAPP_ALLOWED_DM_JIDS=5511988887777,123456789012345",
  "NUCLEUS_TZ=Europe/Oslo",
  "NUCLEUS_LOG=info",
  "NUCLEUS_USER_NAME=Tessa",
].join("\n");
// Synthetic values built at runtime: the committed-secrets scanner reads a
// literal address or home path as real personal data.
const HOME = ["", "Users", "testuser"].join("/");
const MAIL = ["a.person", "corp.invalid"].join("@");
const rules = buildRules(env, "# names\nAcmeCorp\n", HOME);

test("rules split credentials from identifiers and skip configuration", () => {
  assert.deepEqual(rules.credentials, ["tok-abcdefghijklmnopqrstuvwxyz"]);
  assert.ok(rules.identifiers.includes("5511988887777"));
  assert.ok(rules.identifiers.includes(HOME));
  assert.ok(!rules.identifiers.includes("Europe/Oslo"), "a TZ value is configuration");
  assert.ok(!rules.identifiers.includes("Tessa"), "values under 6 characters are skipped, like check-secrets.sh");
  assert.deepEqual(rules.words, ["AcmeCorp"]);
});

test("credentials are redacted in every message", () => {
  const r = filterOutbound("key tok-abcdefghijklmnopqrstuvwxyz and ghp_" + "a".repeat(36), rules, {
    audience: "operator-dm",
    progress: false,
  });
  assert.equal(r.hits.length, 2);
  assert.match(r.text, /^key \[redacted\] and \[redacted\]\n\n\(2 value\(s\) withheld by the secret filter\)$/);
});

test("the operator's DM keeps identifiers in a final reply; a group does not", () => {
  const text = `AcmeCorp standup: call 5511988887777, mail ${MAIL}, file ${HOME}/x`;
  const dm = filterOutbound(text, rules, { audience: "operator-dm", progress: false });
  assert.equal(dm.text, text);
  assert.equal(dm.hits.length, 0);
  const group = filterOutbound(text, rules, { audience: "shared", progress: false });
  assert.doesNotMatch(group.text, /AcmeCorp|5511988887777|a\.person@|testuser/);
  assert.ok(group.hits.includes("denylist"));
});

test("a progress message with any match is withheld; placeholders are not matches", () => {
  const p = filterOutbound("↻ Progress: reading the AcmeCorp notes", rules, { audience: "operator-dm", progress: true });
  assert.equal(p.withheld, true);
  const ok = filterOutbound("↻ Progress: mailing you@example.com", rules, { audience: "operator-dm", progress: true });
  assert.equal(ok.withheld, false);
  assert.equal(ok.hits.length, 0);
});

test("denylist words match whole words only, case-insensitively", () => {
  const r = filterOutbound("acmecorp and AcmeCorporation", rules, { audience: "shared", progress: false });
  assert.match(r.text, /^\[redacted\] and AcmeCorporation/);
});

// Credential shapes shared with core/src/secret_filter.rs. The vector text
// is stored in pieces so no committed file holds a contiguous
// credential-shaped string.
type Piece = string | { repeat: string; times: number };
const assemble = (parts: Piece[]) => parts.map((p) => (typeof p === "string" ? p : p.repeat.repeat(p.times))).join("");

test("credential shapes match the shared vectors", () => {
  const file = path.join(import.meta.dirname, "..", "..", "..", "core", "testdata", "credential_vectors.json");
  const vectors = JSON.parse(fs.readFileSync(file, "utf8")) as Array<{
    name: string;
    text: Piece[];
    secret?: Piece[];
    kind: string | null;
    keep?: string;
  }>;
  assert.ok(vectors.length >= 12);
  for (const v of vectors) {
    const r = redactCredentials(assemble(v.text), { credentials: [] });
    if (v.kind) {
      assert.ok(r.hits.includes(v.kind), `${v.name}: ${JSON.stringify(r)}`);
      assert.ok(!r.text.includes(assemble(v.secret!)), `${v.name}: ${r.text}`);
    } else {
      assert.deepEqual(r.hits, [], `${v.name}: ${r.text}`);
    }
    if (v.keep) assert.ok(r.text.includes(v.keep), `${v.name}: ${r.text}`);
  }
});

test("credential shapes are redacted in every message, including the operator's DM", () => {
  const jwt = ["eyJ", "hbGciOiJIUzI1NiJ9", ".", "eyJ", "zdWIiOiIxMjM0In0", ".", "x".repeat(12)].join("");
  const r = filterOutbound(`token ${jwt}`, rules, { audience: "operator-dm", progress: false });
  assert.ok(r.hits.includes("credential-jwt"));
  assert.ok(!r.text.includes(jwt));
  assert.equal(looksRandom("configuration"), false);
  assert.equal(looksRandom("q8Zr2Tn5Wb4Yx7Kd"), true);
});
