// Caller detection for the send scripts (ADR-033): the shared classification
// vectors, the send policy, and the real process tree — a script run with
// its Nucleus variables removed (`env -u …`) is still recognized from the
// start environment of the `claude` process above it.

import { test } from "node:test";
import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { DatabaseSync } from "node:sqlite";
import { classify, isClaudeExec, sessionIdFromArgs, type Origin, type Snapshot } from "./proc_tree.js";
import { refusal } from "./caller_guard.js";
import { sha256Hex } from "./turn_store.js";

const vectorsPath = path.join(import.meta.dirname, "..", "..", "..", "core", "testdata", "caller_origin_vectors.json");

test("classification matches the shared vectors", () => {
  const vectors = JSON.parse(fs.readFileSync(vectorsPath, "utf8")) as Array<{
    name: string;
    snapshot: Snapshot;
    expect: { origin: string; kind?: string; agent?: string; scope?: string; worker?: string; session_id?: string | null };
  }>;
  assert.ok(vectors.length >= 8);
  for (const v of vectors) {
    const got = classify(v.snapshot);
    assert.equal(got.origin, v.expect.origin, v.name);
    if (got.origin === "nucleus") {
      assert.equal(got.session.kind, v.expect.kind, v.name);
      assert.equal(got.session.agent, v.expect.agent ?? null, v.name);
      assert.equal(got.session.scope, v.expect.scope ?? null, v.name);
      assert.equal(got.session.worker, v.expect.worker ?? null, v.name);
      assert.equal(got.session.sessionId, v.expect.session_id ?? null, v.name);
    }
    if (got.origin === "operator-session") assert.equal(got.sessionId, v.expect.session_id ?? null, v.name);
  }
});

test("claude executables and session ids", () => {
  assert.ok(isClaudeExec("claude", ""));
  assert.ok(isClaudeExec("-claude", ""));
  assert.ok(isClaudeExec("x", "/u/.local/share/claude/versions/2.1.281"));
  assert.ok(!isClaudeExec("/bin/zsh", "/bin/zsh"));
  // The caller's environment does not add names: a command could name any
  // process above it (a tmux server) and have it taken for claude.
  const saved = process.env.NUCLEUS_CLAUDE_BIN;
  process.env.NUCLEUS_CLAUDE_BIN = "/opt/homebrew/bin/tmux";
  try {
    assert.ok(!isClaudeExec("tmux", "/opt/homebrew/bin/tmux"));
  } finally {
    if (saved === undefined) delete process.env.NUCLEUS_CLAUDE_BIN;
    else process.env.NUCLEUS_CLAUDE_BIN = saved;
  }
  assert.equal(sessionIdFromArgs(["--session-id", "abc"]), "abc");
  assert.equal(sessionIdFromArgs(["--resume=def"]), "def");
  assert.equal(sessionIdFromArgs(["--resume"]), null);
});

const nucleus = (kind: string, extra: Partial<{ scope: string; worker: string }> = {}): Origin => ({
  origin: "nucleus",
  session: { pid: 1, kind, agent: "whatsapp", scope: extra.scope ?? null, worker: extra.worker ?? null, sessionId: null },
});

test("send policy: the operator sends; sessions only through their own path", () => {
  const valid = (t: string) => t === "good";
  for (const o of [{ origin: "terminal" }, { origin: "operator-session", sessionId: null }] as Origin[]) {
    for (const a of ["send", "ack", "document"] as const) assert.equal(refusal(a, o, valid), null);
  }
  assert.equal(refusal("ack", nucleus("braindump"), valid), null);
  assert.match(refusal("send", nucleus("braindump"), valid)!, /may not send/);
  assert.equal(refusal("document", nucleus("chat", { scope: "good" }), valid), null);
  assert.match(refusal("document", nucleus("chat", { scope: "stale" }), valid)!, /valid task scope/);
  assert.match(refusal("send", nucleus("chat", { scope: "good" }), valid)!, /may not send/);
  assert.match(refusal("document", nucleus("worker", { worker: "ab12" }), valid)!, /worker/);
  assert.match(refusal("ack", nucleus("agent"), valid)!, /may not send/);
  assert.match(refusal("send", { origin: "detached" }, valid)!, /detached/);
  assert.match(refusal("send", { origin: "unknown", reason: "x" }, valid)!, /cannot be identified/);
});

/** Run `script` under a fake `claude` process started with `env`, detached
 *  from this test's own ancestry (re-parented to launchd, as a tmux pane
 *  process is) so the fake is the outermost claude. The fake is node with
 *  argv[0] "claude": macOS hides the start environment of platform binaries
 *  such as /bin/sh from `ps -E`, and a real claude binary is not one. The
 *  script runs with every Nucleus variable removed. */
function underFakeClaude(env: Record<string, string>, script: string, argv0 = "claude"): { status: number; out: string } {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-fake-claude-"));
  const out = path.join(dir, "out");
  const rc = path.join(dir, "rc");
  const tsx = path.join(import.meta.dirname, "..", "node_modules", ".bin", "tsx");
  const scriptFile = path.join(dir, "s.ts");
  fs.writeFileSync(scriptFile, script);
  const fake = path.join(dir, "fake.cjs");
  fs.writeFileSync(
    fake,
    `const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const until = Date.now() + 10000;
while (process.ppid !== 1 && Date.now() < until) spawnSync("sleep", ["0.05"]);
const strip = ["NUCLEUS_SESSION", "NUCLEUS_AGENT", "NUCLEUS_TASK_SCOPE", "NUCLEUS_TASK_WORKER", "CLAUDE_CODE_SESSION_ID"];
const args = strip.flatMap((k) => ["-u", k]).concat([${JSON.stringify(tsx)}, ${JSON.stringify(scriptFile)}]);
const r = spawnSync("env", args, { encoding: "utf8" });
fs.writeFileSync(${JSON.stringify(out)}, (r.stdout || "") + (r.stderr || ""));
fs.writeFileSync(${JSON.stringify(rc)}, String(r.status));
`,
  );
  spawnSync("/bin/sh", ["-c", `(exec -a ${argv0} '${process.execPath}' '${fake}') &`], {
    env: { ...process.env, ...env },
    stdio: "ignore",
  });
  const start = Date.now();
  while (!fs.existsSync(rc)) {
    if (Date.now() - start > 30_000) throw new Error("the fake session did not finish");
    execFileSync("sleep", ["0.1"]);
  }
  const res = { status: Number(fs.readFileSync(rc, "utf8").trim()), out: fs.readFileSync(out, "utf8") };
  fs.rmSync(dir, { recursive: true, force: true });
  return res;
}

test("a session cannot shed its identity with env -u (the start environment decides)", () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-guard-"));
  const dbPath = path.join(dir, "whatsapp.db");
  const db = new DatabaseSync(dbPath);
  db.exec("CREATE TABLE task_scopes (token_sha256 TEXT PRIMARY KEY, chat_id TEXT NOT NULL, created_at TEXT NOT NULL)");
  db.prepare("INSERT INTO task_scopes VALUES (?, 'c', 'x')").run(sha256Hex("good-token"));
  db.close();
  const guard = path.join(import.meta.dirname, "caller_guard.ts");
  const script = (action: string) =>
    `import { refuseUnlessAllowed } from ${JSON.stringify(guard)};\n` +
    `refuseUnlessAllowed("probe", ${JSON.stringify(action)}, ${JSON.stringify(dbPath)});\nconsole.log("ALLOWED");\n`;

  const worker = underFakeClaude({ NUCLEUS_SESSION: "worker", NUCLEUS_TASK_WORKER: "0123456789abcdef" }, script("send"));
  assert.equal(worker.status, 3, worker.out);
  assert.match(worker.out, /worker session/);

  const chatSend = underFakeClaude({ NUCLEUS_SESSION: "chat", NUCLEUS_AGENT: "whatsapp", NUCLEUS_TASK_SCOPE: "good-token" }, script("send"));
  assert.equal(chatSend.status, 3, chatSend.out);

  const chatDoc = underFakeClaude({ NUCLEUS_SESSION: "chat", NUCLEUS_AGENT: "whatsapp", NUCLEUS_TASK_SCOPE: "good-token" }, script("document"));
  assert.equal(chatDoc.status, 0, chatDoc.out);
  assert.match(chatDoc.out, /ALLOWED/);

  const staleDoc = underFakeClaude({ NUCLEUS_SESSION: "chat", NUCLEUS_TASK_SCOPE: "revoked" }, script("document"));
  assert.equal(staleDoc.status, 3, staleDoc.out);

  const ack = underFakeClaude({ NUCLEUS_SESSION: "braindump", NUCLEUS_AGENT: "whatsapp" }, script("ack"));
  assert.equal(ack.status, 0, ack.out);
  fs.rmSync(dir, { recursive: true, force: true });
});

test("a marked process that is not claude still decides (a tmux server started from a session keeps its marker)", () => {
  const guard = path.join(import.meta.dirname, "caller_guard.ts");
  const dbPath = path.join(fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-guard-")), "whatsapp.db");
  const script =
    `import { refuseUnlessAllowed } from ${JSON.stringify(guard)};\n` +
    `refuseUnlessAllowed("probe", "send", ${JSON.stringify(dbPath)});\nconsole.log("ALLOWED");\n`;
  // argv[0] "tmux-server": not a claude name. Before the marker rule this
  // classified as the operator's terminal (or detached) and was allowed.
  const r = underFakeClaude({ NUCLEUS_SESSION: "worker", NUCLEUS_TASK_WORKER: "0123456789abcdef" }, script, "tmux-server");
  assert.equal(r.status, 3, r.out);
  assert.match(r.out, /worker session/);
});
