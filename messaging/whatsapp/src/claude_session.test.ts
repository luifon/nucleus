import { test } from "node:test";
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { promisify } from "node:util";
import { exec } from "node:child_process";
import { writeFileSync, mkdtempSync, rmSync, readFileSync } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import {
  waitForTuiReady,
  lastNTurns,
  buildPrimingPreamble,
  msUntilNext4am,
  extractLastAssistantText,
  lastPromptRow,
  draftFragment,
  draftStuck,
  waitForDraftGone,
  splitRotationReply,
  Turn,
} from "./claude_session.js";

const execAsync = promisify(exec);

async function tmux(args: string[]): Promise<{ stdout: string; stderr: string }> {
  return new Promise((resolve, reject) => {
    const child = spawn("tmux", args, { stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (b) => (stdout += b.toString()));
    child.stderr.on("data", (b) => (stderr += b.toString()));
    child.on("close", (code) => {
      if (code === 0) resolve({ stdout, stderr });
      else reject(new Error(`tmux ${args.join(" ")} exited ${code}: ${stderr}`));
    });
  });
}

async function tmuxKill(session: string): Promise<void> {
  try {
    await tmux(["kill-session", "-t", session]);
  } catch {
    // not present, fine
  }
}

async function paneContent(target: string): Promise<string> {
  const { stdout } = await tmux(["capture-pane", "-t", target, "-p"]);
  return stdout;
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

// Mirror of core/src/claude_session.rs::wait_for_tui_ready_auto_dismisses_resume_picker.
// Seed a tmux pane with the resume-from-summary picker; once the dismissal
// keys arrive, repaint with a "ready" frame and assert the function returns
// Ok within the timeout.
test("waitForTuiReady auto-dismisses the resume-from-summary picker", async () => {
  const session = "nucleus-ts-tui-ready-test";
  await tmuxKill(session);

  await tmux(["new-session", "-d", "-s", session, "cat"]);
  const target = `${session}:0`;
  try {
    const seed = "❯ Resume from summary?\n  1. Resume from summary\n  2. Start fresh\n";
    await tmux(["send-keys", "-t", target, seed]);
    await sleep(150);

    // Painter: once we see the "1" the dismiss path sent, clear and write
    // a ready frame. The `cat` process echoes typed keys, so the "1" lands
    // in the pane buffer where capture-pane can see it.
    const painter = (async () => {
      for (let i = 0; i < 40; i++) {
        const pane = await paneContent(target);
        if (pane.includes("1\n") || pane.match(/\n1\n/)) {
          await tmux(["send-keys", "-t", target, "C-l"]);
          await tmux(["send-keys", "-t", target, "❯ ready\nTry asking me something\nauto mode on\n"]);
          return;
        }
        await sleep(100);
      }
    })();

    await waitForTuiReady(target, 8000);
    await painter;
  } finally {
    await tmuxKill(session);
  }
});

test("lastNTurns filters tool entries, system injections, and date preamble", () => {
  const tmp = mkdtempSync(path.join(os.tmpdir(), "nucleus-ts-last-n-"));
  const file = path.join(tmp, "transcript.jsonl");
  try {
    const lines = [
      `{"type":"permission-mode","permissionMode":"auto"}`,
      `{"type":"file-history-snapshot","messageId":"abc"}`,
      // System-injected user turn — must be skipped.
      `{"type":"user","message":{"role":"user","content":[{"type":"text","text":"<ide_opened_file>some log</ide_opened_file>"}]}}`,
      // Date-preamble wrapped real user message.
      `{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[context: today is 2026-05-23 (Sat), local 09:00 BRT]\\n\\nhello there"}]}}`,
      // Assistant thinking + tool_use → ignored (no text block).
      `{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"…"},{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}`,
      // Tool result tagged role:user → must be skipped.
      `{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}`,
      // Assistant text reply.
      `{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi! how can I help?"}]}}`,
      // User message with string-form content.
      `{"type":"user","message":{"role":"user","content":"second user message"}}`,
      // Assistant reply combining thinking + text.
      `{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"…"},{"type":"text","text":"second assistant reply"}]}}`,
    ];
    writeFileSync(file, lines.join("\n"));
    const turns = lastNTurns(file, 10);
    assert.equal(turns.length, 4);
    assert.deepEqual(turns[0], { role: "user", text: "hello there" });
    assert.deepEqual(turns[1], { role: "assistant", text: "hi! how can I help?" });
    assert.deepEqual(turns[2], { role: "user", text: "second user message" });
    assert.deepEqual(turns[3], { role: "assistant", text: "second assistant reply" });
  } finally {
    rmSync(tmp, { recursive: true, force: true });
  }
});

test("lastNTurns caps at n and missing files return empty", () => {
  const tmp = mkdtempSync(path.join(os.tmpdir(), "nucleus-ts-cap-"));
  try {
    const file = path.join(tmp, "t.jsonl");
    const lines: string[] = [];
    for (let i = 0; i < 15; i++) {
      lines.push(
        `{"type":"user","message":{"role":"user","content":"u${i}"}}`,
      );
      lines.push(
        `{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"a${i}"}]}}`,
      );
    }
    writeFileSync(file, lines.join("\n"));
    const turns = lastNTurns(file, 10);
    assert.equal(turns.length, 10);
    assert.equal(turns[0].text, "u10");
    assert.equal(turns[9].text, "a14");

    const missing = lastNTurns(path.join(tmp, "nope.jsonl"), 10);
    assert.equal(missing.length, 0);
  } finally {
    rmSync(tmp, { recursive: true, force: true });
  }
});

test("buildPrimingPreamble orders summary then replay then closing line", () => {
  const replay: Turn[] = [
    { role: "user", text: "hello" },
    { role: "assistant", text: "hi there" },
  ];
  const out = buildPrimingPreamble("- did X\n- decided Y", replay);
  const summaryIdx = out.indexOf("- did X");
  const userIdx = out.indexOf("USER: hello");
  const asstIdx = out.indexOf("ASSISTANT: hi there");
  const closingIdx = out.indexOf("End of priming");
  assert.ok(summaryIdx >= 0 && userIdx >= 0 && asstIdx >= 0 && closingIdx >= 0);
  assert.ok(summaryIdx < userIdx);
  assert.ok(userIdx < asstIdx);
  assert.ok(asstIdx < closingIdx);
});

test("msUntilNext4am wraps correctly", () => {
  // 03:30 UTC → 30 minutes to next 04:00 UTC.
  const at0330 = new Date(Date.UTC(2026, 4, 23, 3, 30, 0));
  assert.equal(msUntilNext4am(at0330, "UTC"), 30 * 60 * 1000);

  // 04:30 UTC → 23h30m until next 04:00 UTC.
  const at0430 = new Date(Date.UTC(2026, 4, 23, 4, 30, 0));
  assert.equal(msUntilNext4am(at0430, "UTC"), (23 * 60 + 30) * 60 * 1000);

  // Exactly 04:00 UTC → 24h until next 04:00 (we want the *next* one,
  // not "right now").
  const at0400 = new Date(Date.UTC(2026, 4, 23, 4, 0, 0));
  assert.equal(msUntilNext4am(at0400, "UTC"), 24 * 60 * 60 * 1000);
});

test("waitForTuiReady times out on an unknown stuck prompt", async () => {
  const session = "nucleus-ts-tui-ready-test-2";
  await tmuxKill(session);
  await tmux(["new-session", "-d", "-s", session, "cat"]);
  const target = `${session}:0`;
  try {
    await tmux([
      "send-keys",
      "-t",
      target,
      "Choose the credential to use:\n  1. account-a\n  2. account-b\n",
    ]);
    let err: unknown;
    try {
      await waitForTuiReady(target, 800);
    } catch (e) {
      err = e;
    }
    assert.ok(err instanceof Error, "expected timeout error");
    assert.match((err as Error).message, /did not become ready/);
  } finally {
    await tmuxKill(session);
  }
});

// --- end-of-turn gating (awaitTurnComplete) — braindump narration-leak regression ---

// A transcript where the model emits a pre-tool narration line (stop_reason
// "tool_use"), then a tool result, then the real JSON reply (stop_reason
// "end_turn"). This is the shape that returned "Ack posted. Reading the two
// reference braindumps…" as a braindump plan on 2026-05-28.
const NARRATION_THEN_JSON = [
  JSON.stringify({
    type: "assistant",
    message: { stop_reason: "tool_use", content: [{ type: "text", text: "Ack posted. Reading the two reference braindumps to mirror their structure." }] },
  }),
  JSON.stringify({
    type: "assistant",
    message: { stop_reason: "tool_use", content: [{ type: "tool_use", name: "Read", input: {} }] },
  }),
  JSON.stringify({ type: "user", message: { content: [{ type: "tool_result", content: "..." }] } }),
  JSON.stringify({
    type: "assistant",
    message: { stop_reason: "end_turn", content: [{ type: "text", text: '{"ops":[],"summary":"nothing to file","confidence":0.9}' }] },
  }),
].join("\n");

test("extractLastAssistantText (default) returns the last assistant text — incl. mid-turn narration", () => {
  // The pre-fix quiescence path: if the model paused after the narration line,
  // THIS is what got returned and failed JSON parsing.
  const buffer = NARRATION_THEN_JSON.split("\n").slice(0, 2).join("\n");
  assert.equal(
    extractLastAssistantText(buffer),
    "Ack posted. Reading the two reference braindumps to mirror their structure.",
  );
});

test("extractLastAssistantText(requireEndTurn) skips tool_use narration, returns the end_turn JSON", () => {
  assert.equal(
    extractLastAssistantText(NARRATION_THEN_JSON, true),
    '{"ops":[],"summary":"nothing to file","confidence":0.9}',
  );
});

test("extractLastAssistantText(requireEndTurn) returns null until the turn actually ends", () => {
  // Only the narration + tool_use seen so far — no end_turn yet. awaitTurnComplete
  // keeps waiting (bounded by maxWaitMs) instead of returning the narration.
  const partial = NARRATION_THEN_JSON.split("\n").slice(0, 3).join("\n");
  assert.equal(extractLastAssistantText(partial, true), null);
});

// ---------------------------------------------------------------------------
// Verified-submit helpers (mirror of core/src/claude_session.rs). The vector
// file is SHARED with the Rust tests — add cases there, never fork
// per-language expectations.
// ---------------------------------------------------------------------------

const VECTORS = JSON.parse(
  readFileSync(
    path.join(
      path.dirname(fileURLToPath(import.meta.url)),
      "../../../core/testdata/submit_verify_vectors.json",
    ),
    "utf8",
  ),
);

test("lastPromptRow matches the shared submit-verify vectors", () => {
  assert.ok(VECTORS.last_prompt_row.length > 0);
  for (const c of VECTORS.last_prompt_row) {
    assert.equal(lastPromptRow(c.pane), c.expect, `vector: ${c.name}`);
  }
});

test("draftFragment matches the shared submit-verify vectors", () => {
  assert.ok(VECTORS.draft_fragment.length > 0);
  for (const c of VECTORS.draft_fragment) {
    assert.equal(draftFragment(c.content), c.expect, `vector: ${c.name}`);
  }
});

test("draftStuck matches the shared submit-verify vectors", () => {
  assert.ok(VECTORS.draft_stuck.length > 0);
  for (const c of VECTORS.draft_stuck) {
    assert.equal(draftStuck(c.pane, c.fragment), c.expect, `vector: ${c.name}`);
  }
});

// Mirror of core's wait_for_draft_gone_detects_stuck_then_cleared: a real
// tmux pane still showing our draft on the live ❯ row must report NOT gone
// at the deadline (the 2026-07-18 eaten-Enter shape), then flip to gone once
// an empty prompt row appears on a fresh line (the old draft stays above as
// history — exactly the post-submit pane shape lastPromptRow must skip).
test("waitForDraftGone detects a stuck draft, then a cleared live row", async () => {
  const session = "nucleus-ts-draft-gone-test";
  await tmuxKill(session);
  await tmux(["new-session", "-d", "-s", session, "cat"]);
  const target = `${session}:0`;
  try {
    await tmux(["send-keys", "-t", target, "-l", "❯ remind me tomorrow at 9am"]);
    await new Promise((r) => setTimeout(r, 200));

    const stuck = await waitForDraftGone(target, "remind me tomorrow at 9", 900);
    assert.equal(stuck, false, "draft on the live row must report NOT gone");

    await tmux(["send-keys", "-t", target, "Enter"]);
    await tmux(["send-keys", "-t", target, "-l", "❯ "]);
    const gone = await waitForDraftGone(target, "remind me tomorrow at 9", 5000);
    assert.equal(gone, true, "cleared live row must report gone");
  } finally {
    await tmuxKill(session);
  }
});

// Mirror of core's rotation_reply_vectors_split (ADR-025). Shared vector
// file — add cases there, never fork per-language expectations.
const ROTATION_VECTORS = JSON.parse(
  readFileSync(
    path.join(
      path.dirname(fileURLToPath(import.meta.url)),
      "../../../core/testdata/rotation_reply_vectors.json",
    ),
    "utf8",
  ),
);

test("splitRotationReply matches the shared rotation-reply vectors", () => {
  assert.ok(ROTATION_VECTORS.split_rotation_reply.length > 0);
  for (const c of ROTATION_VECTORS.split_rotation_reply) {
    const { summary, durable } = splitRotationReply(c.reply);
    assert.equal(summary, c.expect_summary, `summary vector: ${c.name}`);
    assert.equal(durable, c.expect_durable, `durable vector: ${c.name}`);
  }
});
