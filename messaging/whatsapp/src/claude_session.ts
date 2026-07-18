// Long-lived interactive `claude` sessions driven via tmux.
//
// TS counterpart of `nucleus_core::claude_session` — same architecture:
// spawn `claude` in a tmux window, send messages via paste-buffer,
// tail the session transcript JSONL for assistant turns. No TUI scraping.

import { spawn, exec } from "node:child_process";
import { promisify } from "node:util";
import { promises as fs } from "node:fs";
import { readFileSync } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { randomUUID } from "node:crypto";

import { appendEntry as diaryAppendEntry, record as diaryRecord } from "./diary.js";
import * as runlog from "./runlog.js";

const execAsync = promisify(exec);

export interface SpawnOptions {
  workspaceRoot: string;
  appendSystemPrompt?: string;
  permissionMode?: string;
  disallowedTools?: string[];
  /** Tool patterns to pre-approve so the auto-mode classifier doesn't
   *  prompt or block them. Same pattern syntax as disallowedTools,
   *  e.g. `Bash(npm test:*)`. */
  allowedTools?: string[];
  addDirs?: string[];
  tmuxSession: string;
  windowName?: string;
  /** ms to wait for the TUI input prompt to appear. */
  readyTimeoutMs?: number;
  /** If set, resume that existing claude session via `--resume`. */
  resumeSessionId?: string;
  /** Registry agent name (ADR-016). When set, each spawn appends a row to
   *  `memory/logs/<agent>/runs.jsonl` pointing at the transcript. */
  agentLabel?: string;
}

export interface AskOptions {
  maxWaitMs?: number;
  /** "No new transcript lines for this long" → claude is done. */
  quiescentMs?: number;
  /** Only return once the model's turn actually ended (an assistant
   *  message with `stop_reason: "end_turn"`), instead of returning the
   *  last assistant text after `quiescentMs` of silence. Set this for
   *  agentic, multi-step asks (read context → call tools → produce a
   *  final JSON/answer): without it, a narration line emitted before a
   *  tool call — which carries `stop_reason: "tool_use"`, not
   *  `end_turn` — gets returned as the reply if the model then pauses
   *  >quiescentMs (e.g. reading files). That's how a braindump plan came
   *  back as "Ack posted. Reading the two reference braindumps…" instead
   *  of the ops JSON. Mirrors nucleus_core's `await_turn_complete`
   *  (cfe6238). Bounded by `maxWaitMs`; on timeout we throw rather than
   *  return mid-turn narration. */
  awaitTurnComplete?: boolean;
}

export interface AskResult {
  reply: string;
  sessionId: string;
  elapsedMs: number;
  wasColdSpawn: boolean;
  /** Absolute path to the session transcript (ADR-016/017). */
  transcriptPath: string;
  /** True when this ask crossed reviewNudgeInterval for the chat (ADR-017). */
  reviewDue: boolean;
}

const DEFAULT_ASK: Required<AskOptions> = {
  maxWaitMs: 180_000,
  quiescentMs: 3_000,
  awaitTurnComplete: false,
};

/** Prepend a fresh wall-clock context line to every payload. Long-lived
 *  SessionPool sessions otherwise stay anchored to spawn-day "today" —
 *  the model has no built-in clock, and a single `date` call at session
 *  start gets carried as the anchor for every turn after. Recomputing
 *  per ask() keeps "tomorrow"/"in N hours" reasoning honest. */
function withDatePreamble(message: string): string {
  const now = new Date();
  const tz = process.env.TZ || process.env.NUCLEUS_TZ || "America/Sao_Paulo";
  const fmt = new Intl.DateTimeFormat("en-CA", {
    timeZone: tz,
    weekday: "short",
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
  const parts = Object.fromEntries(fmt.formatToParts(now).map((p) => [p.type, p.value]));
  const stamp = `${parts.year}-${parts.month}-${parts.day} (${parts.weekday}), local ${parts.hour}:${parts.minute} ${tz}`;
  return `[context: today is ${stamp}]\n\n${message}`;
}

/** A live tmux-hosted claude session. */
export class Session {
  constructor(
    public readonly sessionId: string,
    public readonly tmuxTarget: string,
    public readonly transcriptPath: string,
    private cursor: number,
    // Run-log bookkeeping (ADR-016); set when spawned with an agentLabel.
    private readonly workspaceRoot: string = "",
    private readonly agentLabel?: string,
    private readonly runId: string = "",
  ) {}

  static async spawn(opts: SpawnOptions): Promise<Session> {
    const resuming = !!opts.resumeSessionId;
    const sessionId = opts.resumeSessionId ?? randomUUID();
    const windowName = opts.windowName ?? sessionId.slice(0, 8);

    await ensureTmuxSession(opts.tmuxSession);

    // Launch with the configured/default model first; if it boots into a
    // fatal model-unavailable banner, retry ONCE with the fallback model
    // (fable-5 incident 2026-06-13: a bad default left sessions hung at the
    // error banner for the full timeout — the banner isn't an assistant
    // turn, so transcript-tailing never sees it). Both attempts kill their
    // window on any failure so retries can't leak windows.
    const fb = fallbackModel();
    let target = await launchWindow(opts, sessionId, resuming, windowName, undefined);
    if (target === null) {
      console.error(
        `whatsapp: session spawn — configured/default model unavailable, retrying with fallback ${fb}`,
      );
      target = await launchWindow(opts, sessionId, resuming, windowName, fb);
      if (target === null) {
        throw new Error(
          `session spawn: both the configured/default model and the fallback model ${fb} are ` +
            `unavailable (set NUCLEUS_CLAUDE_FALLBACK_MODEL to a model you can use)`,
        );
      }
    }

    const transcriptPath = transcriptPathFor(opts.workspaceRoot, sessionId);
    // CRITICAL: when --resume'ing, the transcript file already has all the
    // prior turns. If we start reading from offset 0, waitForAssistant
    // sees them as "current" content, marks haveAssistant=true on the
    // first poll, then triggers the quiescent extractor after 3s of
    // (silent) new-bytes-waiting — pulling the LAST historical assistant
    // text instead of the response to the current ask. Pin the cursor to
    // the file's current size at spawn time so we only ever consider
    // content appended AFTER this Session was created.
    let initialCursor = 0;
    if (resuming) {
      try {
        const stat = await fs.stat(transcriptPath);
        initialCursor = stat.size;
      } catch {
        // No transcript file yet (rare on resume but possible).
      }
    }

    // Run-log: append an in-flight row so the transcript is recoverable
    // after the window is killed (ADR-016). Best-effort.
    const runId = randomUUID();
    if (opts.agentLabel) {
      await runlog
        .recordStart(opts.workspaceRoot, {
          run_id: runId,
          agent: opts.agentLabel,
          session_id: sessionId,
          transcript_path: transcriptPath,
          tmux_target: target,
          started_at: new Date().toISOString(),
          ended_at: null,
          ok: null,
        })
        .catch(() => {});
    }

    return new Session(
      sessionId,
      target,
      transcriptPath,
      initialCursor,
      opts.workspaceRoot,
      opts.agentLabel,
      runId,
    );
  }

  async ask(message: string, opts: AskOptions = {}): Promise<string> {
    const ask = { ...DEFAULT_ASK, ...opts };
    const fromOffset = this.cursor;
    try {
      await pasteAndSend(this.tmuxTarget, withDatePreamble(message));
    } catch (e) {
      if (e instanceof WedgedInputError) {
        // The TUI stopped accepting submits (2026-07-18: operator DMs piled
        // up typed-but-unsent, invisibly). A wedged window is unrecoverable
        // from outside — kill it so isAlive() fails and the pool respawns
        // with --resume, and rethrow so the caller knows THIS turn was lost
        // instead of timing out against a black hole.
        await this.close().catch(() => {});
      }
      throw e;
    }
    const reply = await waitForAssistant(
      this.transcriptPath,
      fromOffset,
      ask.maxWaitMs,
      ask.quiescentMs,
      ask.awaitTurnComplete,
    );
    try {
      const stat = await fs.stat(this.transcriptPath);
      this.cursor = stat.size;
    } catch {
      // best-effort; cursor stays
    }
    return reply;
  }

  /** True while the underlying tmux window still exists. A window can die
   *  without the pool noticing (claude crash, manual kill, `claude update`
   *  swapping the binary, operator cleanup) — callers must check before
   *  reusing a pooled session instead of timing out against a ghost. */
  async isAlive(): Promise<boolean> {
    try {
      await tmux(["display-message", "-p", "-t", this.tmuxTarget, "ok"]);
      return true;
    } catch {
      return false;
    }
  }

  async close(): Promise<void> {
    // Finalize the run-log row (ok = closed cleanly; crashed runs leave
    // ended_at null). Best-effort. See the Rust counterpart in close().
    if (this.agentLabel) {
      await runlog
        .recordEnd(this.workspaceRoot, this.agentLabel, this.runId, true)
        .catch(() => {});
    }
    await tmux(["kill-window", "-t", this.tmuxTarget]).catch(() => {});
  }
}

/** Manages a Map<chatKey, Session>. One claude per chat, lazily spawned. */
export class SessionPool {
  private entries = new Map<
    string,
    { session: Session; lastActive: number; lock: Promise<void>; turnsSinceReview: number }
  >();
  constructor(private readonly config: PoolConfig) {}

  async ask(
    chatKey: string,
    message: string,
    resumeSessionId: string | undefined,
    opts: AskOptions = {},
  ): Promise<AskResult> {
    const t0 = Date.now();

    let entry = this.entries.get(chatKey);
    let wasColdSpawn = false;
    // A pooled session whose tmux window has died (claude crash, binary
    // upgrade, manual kill) must be dropped and respawned — asking into a
    // dead window just burns the full ask timeout. Resume the same claude
    // session id so the conversation continues where it left off.
    if (entry && !(await entry.session.isAlive())) {
      this.entries.delete(chatKey);
      const deadSessionId = entry.session.sessionId;
      await entry.session.close().catch(() => {});
      entry = undefined;
      resumeSessionId = deadSessionId;
    }
    if (!entry) {
      wasColdSpawn = true;
      const session = await Session.spawn({
        workspaceRoot: this.config.workspaceRoot,
        appendSystemPrompt: this.config.appendSystemPrompt,
        permissionMode: this.config.permissionMode,
        disallowedTools: this.config.disallowedTools,
        allowedTools: this.config.allowedTools,
        addDirs: this.config.addDirs,
        tmuxSession: this.config.tmuxSession,
        windowName: sanitizeWindowName(chatKey),
        // 20s (the SpawnOptions default) is too tight for a cold `claude`
        // boot under load; the rotation path already learned this and uses
        // 60s. Keep the two in lockstep.
        readyTimeoutMs: 60_000,
        resumeSessionId,
        agentLabel: this.config.agentLabel,
      });
      entry = { session, lastActive: Date.now(), lock: Promise.resolve(), turnsSinceReview: 0 };
      this.entries.set(chatKey, entry);
    }

    // Serialize per-chat asks — claude can't handle two prompts in flight.
    const prev = entry.lock;
    let release!: () => void;
    entry.lock = new Promise<void>((r) => (release = r));
    await prev;
    try {
      const reply = await entry.session.ask(message, opts);
      entry.lastActive = Date.now();
      // On-the-fly skill-review nudge (ADR-017).
      let reviewDue = false;
      const interval = this.config.reviewNudgeInterval ?? 0;
      if (interval > 0) {
        entry.turnsSinceReview += 1;
        if (entry.turnsSinceReview >= interval) {
          reviewDue = true;
          entry.turnsSinceReview = 0;
        }
      }
      return {
        reply,
        sessionId: entry.session.sessionId,
        elapsedMs: Date.now() - t0,
        wasColdSpawn,
        transcriptPath: entry.session.transcriptPath,
        reviewDue,
      };
    } finally {
      release();
    }
  }

  async reapIdle(): Promise<number> {
    const cutoff = Date.now() - this.config.idleTimeoutMs;
    const stale: string[] = [];
    for (const [key, entry] of this.entries) {
      if (entry.lastActive < cutoff) stale.push(key);
    }
    for (const key of stale) {
      const entry = this.entries.get(key);
      if (entry) {
        this.entries.delete(key);
        await entry.session.close().catch(() => {});
      }
    }
    return stale.length;
  }

  // Roll every active per-chat session forward by one day. TS mirror of
  // `nucleus_core::claude_session::SessionPool::daily_rotate`. Skips
  // chats inactive >24h or with <10 text turns; for the rest, asks the
  // old session to summarize itself, appends the summary to the agent's
  // daily diary, spawns a fresh session primed with summary + last 10
  // turns, and calls `dbUpdate(chatKey, newSessionId)` so the caller
  // can persist the new mapping. Failures are recorded to the diary as
  // OBSERVATION; the old session is left alone in that case.
  async dailyRotate(
    diaryRoot: string,
    dbUpdate: (chatKey: string, newSessionId: string) => Promise<void>,
  ): Promise<RotationStats> {
    const stats: RotationStats = { considered: 0, rotated: 0, skipped: 0, failed: 0 };
    const keys = Array.from(this.entries.keys());
    for (const chatKey of keys) {
      stats.considered++;
      const outcome = await this.rotateOne(chatKey, diaryRoot, dbUpdate).catch((e) => {
        diaryRecord(
          diaryRoot,
          `daily_rotate ${chatKey}`,
          `rotation failed: ${e instanceof Error ? e.message : String(e)}`,
          "OBSERVATION",
        );
        return "failed" as const;
      });
      if (outcome === "rotated") stats.rotated++;
      else if (outcome === "skipped") stats.skipped++;
      else stats.failed++;
    }
    return stats;
  }

  private async rotateOne(
    chatKey: string,
    diaryRoot: string,
    dbUpdate: (chatKey: string, newSessionId: string) => Promise<void>,
  ): Promise<"rotated" | "skipped"> {
    const entry = this.entries.get(chatKey);
    if (!entry) return "skipped";

    // Acquire the per-entry lock the same way ask() does: chain a new
    // promise behind the previous one so a concurrent user ask waits
    // for the rotation to finish.
    const prev = entry.lock;
    let release!: () => void;
    entry.lock = new Promise<void>((r) => (release = r));
    await prev;
    try {
      // Skip cold chats — idle reaper handles them.
      if (entry.lastActive < Date.now() - 24 * 60 * 60 * 1000) return "skipped";

      const turns = lastNTurns(entry.session.transcriptPath, 100);
      if (turns.length < 10) return "skipped";
      const replay = turns.slice(-10);

      // Step 1: ask for the summary. Generous timeout — no user is
      // waiting and the model may have to consume a large transcript.
      const summary = await entry.session.ask(SUMMARY_PROMPT, {
        maxWaitMs: 300_000,
        quiescentMs: 5_000,
      });

      // Step 2: append to today's diary.
      diaryAppendEntry(
        diaryRoot,
        `daily_rotate ${chatKey}`,
        `Session rotated. Yesterday's summary:\n\n${summary.trim()}`,
      );

      // Step 3: spawn a fresh session (no resume, new UUID, new window).
      const newSession = await Session.spawn({
        workspaceRoot: this.config.workspaceRoot,
        appendSystemPrompt: this.config.appendSystemPrompt,
        permissionMode: this.config.permissionMode,
        disallowedTools: this.config.disallowedTools,
        allowedTools: this.config.allowedTools,
        addDirs: this.config.addDirs,
        tmuxSession: this.config.tmuxSession,
        // windowName left undefined → derives from the new session UUID.
        // (Cosmetic only — windows are addressed by unique id, so a name
        // collision with the still-alive old window wouldn't matter.)
        readyTimeoutMs: 60_000,
        agentLabel: this.config.agentLabel,
      });

      // Step 4: prime the new session. If priming fails, tear it down so
      // we don't orphan it.
      const priming = buildPrimingPreamble(summary, replay);
      try {
        await newSession.ask(priming, { maxWaitMs: 300_000, quiescentMs: 5_000 });
      } catch (e) {
        await newSession.close().catch(() => {});
        throw e;
      }

      // Step 5: hand the new session-id to the caller for DB persistence.
      try {
        await dbUpdate(chatKey, newSession.sessionId);
      } catch (e) {
        await newSession.close().catch(() => {});
        throw e;
      }

      // Step 6: swap in the new session, then close the old one.
      const oldSession = entry.session;
      entry.session = newSession;
      entry.lastActive = Date.now();
      await oldSession.close().catch(() => {});

      return "rotated";
    } finally {
      release();
    }
  }

  async shutdown(): Promise<void> {
    for (const [, entry] of this.entries) {
      await entry.session.close().catch(() => {});
    }
    this.entries.clear();
    await tmux(["kill-session", "-t", this.config.tmuxSession]).catch(() => {});
  }
}

export interface PoolConfig {
  workspaceRoot: string;
  appendSystemPrompt?: string;
  permissionMode?: string;
  disallowedTools?: string[];
  /** Tool patterns pre-approved past the auto-mode classifier (ADR-018:
   *  the DM pool pre-approves the docs/enqueue-media CLIs). */
  allowedTools?: string[];
  addDirs?: string[];
  tmuxSession: string;
  idleTimeoutMs: number;
  /** Registry agent name for the run-log (ADR-016), threaded into every
   *  session this pool spawns. */
  agentLabel?: string;
  /** On-the-fly skill review (ADR-017): after this many asks on a chat, the
   *  next AskResult.reviewDue is true. 0/undefined = disabled. */
  reviewNudgeInterval?: number;
}

// ---- internals ----

function transcriptPathFor(workspaceRoot: string, sessionId: string): string {
  const encoded = workspaceRoot.replace(/\//g, "-");
  return path.join(os.homedir(), ".claude", "projects", encoded, `${sessionId}.jsonl`);
}

async function ensureTmuxSession(name: string): Promise<void> {
  try {
    await execAsync(`tmux has-session -t ${shellQuote(name)}`);
    return;
  } catch {
    // not there
  }
  await execAsync(`tmux new-session -d -s ${shellQuote(name)}`);
}

/** Fallback model for spawned sessions when the configured/default model is
 *  unavailable (fable-5 incident 2026-06-13). Mirrors core's fallback_model;
 *  default is the stable Opus the error banner itself recommends. */
function fallbackModel(): string {
  const v = process.env.NUCLEUS_CLAUDE_FALLBACK_MODEL?.trim();
  return v ? v : "claude-opus-4-8";
}

/** True if the pane shows a fatal model-unavailable banner — booted but
 *  can't serve inference, so it would hang at ask time. Claude Code's own
 *  error strings; checked only pre-first-ask so content can't false-trip. */
export function paneShowsModelError(pane: string): boolean {
  return (
    pane.includes("is currently unavailable") ||
    pane.includes("issue with the selected model") ||
    pane.includes("you may not have access to it")
  );
}

/** Create one tmux window running claude, dismiss the trust prompt, wait for
 *  the TUI, and check for a fatal model-unavailable banner. Returns the
 *  window target, or null if the model is unavailable (window killed so the
 *  caller can retry with the fallback). Throws on hard spawn failure
 *  (window killed). `modelOverride` passes --model; undefined uses the
 *  configured/default model. */
async function launchWindow(
  opts: SpawnOptions,
  sessionId: string,
  resuming: boolean,
  windowName: string,
  modelOverride: string | undefined,
): Promise<string | null> {
  const args: string[] = resuming
    ? ["--resume", sessionId]
    : ["--session-id", sessionId];
  // Fallback-model retry only: normal spawns pass no --model and inherit
  // the operator's configured/default model.
  if (modelOverride) args.push("--model", modelOverride);
  if (opts.permissionMode) args.push("--permission-mode", opts.permissionMode);
  if (opts.appendSystemPrompt) args.push("--append-system-prompt", opts.appendSystemPrompt);
  for (const d of opts.addDirs ?? []) args.push("--add-dir", d);
  if (opts.disallowedTools?.length) {
    args.push("--disallowed-tools", opts.disallowedTools.join(" "));
  }
  if (opts.allowedTools?.length) {
    args.push("--allowed-tools", opts.allowedTools.join(" "));
  }

  const inner = `cd ${shellQuote(opts.workspaceRoot)} && claude ${args.map(shellQuote).join(" ")}`;

  // Target the window by its server-unique id (`@N`), never by
  // `session:name` — stale windows share chat-key names and tmux refuses
  // ambiguous matches (2026-06-11 DM outage). The name is cosmetic.
  const { stdout: windowIdRaw } = await tmux([
    "new-window",
    "-t",
    opts.tmuxSession,
    "-n",
    windowName,
    "-P",
    "-F",
    "#{window_id}",
    inner,
  ]);
  const target = windowIdRaw.trim();
  if (!/^@\d+$/.test(target)) {
    throw new Error(`tmux new-window returned unexpected window id: ${JSON.stringify(target)}`);
  }

  try {
    await dismissTrustPrompt(target, 5_000);
    await waitForTuiReady(target, opts.readyTimeoutMs ?? 20_000);
  } catch (e) {
    // Stuck on an undismissable prompt — kill so the next retry doesn't
    // leak an orphan into the tmux session.
    await tmux(["kill-window", "-t", target]).catch(() => {});
    throw e;
  }
  // Extra beat for cursor positioning — also lets the model-unavailable
  // banner finish rendering before we check.
  await sleep(500);

  const { stdout: pane } = await tmux(["capture-pane", "-t", target, "-p"]).catch(() => ({
    stdout: "",
    stderr: "",
  }));
  if (paneShowsModelError(pane)) {
    await tmux(["kill-window", "-t", target]).catch(() => {});
    return null;
  }
  return target;
}

async function tmux(args: string[]): Promise<{ stdout: string; stderr: string }> {
  return new Promise((resolve, reject) => {
    const child = spawn("tmux", args, { stdio: ["ignore", "pipe", "pipe"] });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (d) => (stdout += d.toString()));
    child.stderr.on("data", (d) => (stderr += d.toString()));
    child.on("error", reject);
    child.on("close", (code) => {
      if (code !== 0) {
        reject(new Error(`tmux ${args.join(" ")} exited ${code}: ${stderr.trim()}`));
        return;
      }
      resolve({ stdout, stderr });
    });
  });
}

/** The TUI stopped accepting submits — Enter is being eaten and typed input
 *  accumulates unsent. Callers should kill the window and respawn. */
export class WedgedInputError extends Error {
  constructor(target: string, attempts: number) {
    super(
      `input wedged: submit did not clear after ${attempts} recovery attempts (target ${target})`,
    );
    this.name = "WedgedInputError";
  }
}

/** Load `content` into a fresh NAMED tmux buffer and paste it into `target`. */
async function pasteInto(target: string, content: string): Promise<void> {
  // NAMED buffer per paste, never the server-global default. Concurrent
  // sessions (S13 fires enrich + act/import jobs at once on one inbound)
  // each load-buffer then paste-buffer — on the shared default buffer the
  // second load clobbers the first, so both windows paste whichever load
  // won (the S13 vault-import got the enrich prompt and silently failed,
  // 2026-06-13). A unique buffer name isolates them; `paste-buffer -d`
  // deletes it after so we don't leak buffers.
  const buf = `nucleus-${target.replace(/@/, "")}-${randomUUID().slice(0, 8)}`;
  // Load buffer from stdin so any content (quotes, emoji, newlines) is safe.
  await new Promise<void>((resolve, reject) => {
    const child = spawn("tmux", ["load-buffer", "-b", buf, "-"], {
      stdio: ["pipe", "ignore", "pipe"],
    });
    let stderr = "";
    child.stderr.on("data", (d) => (stderr += d.toString()));
    child.on("error", reject);
    child.on("close", (code) =>
      code === 0 ? resolve() : reject(new Error(`load-buffer failed: ${stderr.trim()}`)),
    );
    child.stdin!.write(content);
    child.stdin!.end();
  });
  await tmux(["paste-buffer", "-d", "-b", buf, "-t", target]);
}

/** Close-bracketed-paste escape, sent literally. If a paste ever leaves the
 *  TUI mid-paste-mode, every later keystroke (including Enter) is swallowed
 *  as literal pasted text — this terminator snaps it out. */
const BRACKETED_PASTE_END = "\x1b[201~";

async function pasteAndSend(target: string, content: string): Promise<void> {
  await pasteInto(target, content);
  // Wait for the bracketed-paste sequence to fully drain into claude's TUI
  // before pressing Enter. Without this, large pastes leave the TUI in
  // mid-paste-mode when Enter arrives, so the Enter gets eaten as a literal
  // newline and the prompt sits queued unsent. Same fix as the Rust side
  // (core/src/claude_session.rs::wait_for_input_settled).
  await waitForInputSettled(target, 250, 10_000);

  // Submit is VERIFIED, not fire-and-forget: on 2026-07-18 the settle
  // heuristic passed, Enter was eaten anyway, and the operator's DMs piled
  // up typed-but-unsent with zero signal. Ladder of increasingly forceful
  // recoveries; every rung ends with Enter + "did the input row clear?".
  const recoveries: (() => Promise<void>)[] = [
    async () => {}, // rung 0: plain Enter
    async () => {
      // rung 1: close a possibly-stuck bracketed paste, then Enter
      await tmux(["send-keys", "-t", target, "-l", BRACKETED_PASTE_END]);
    },
    async () => {
      // rung 2: clear the draft entirely and re-paste from scratch
      await tmux(["send-keys", "-t", target, "-l", BRACKETED_PASTE_END]).catch(() => {});
      await tmux(["send-keys", "-t", target, "C-u"]);
      await pasteInto(target, content);
      await waitForInputSettled(target, 250, 10_000);
    },
  ];
  const fragment = draftFragment(content);
  for (const recover of recoveries) {
    await recover();
    await tmux(["send-keys", "-t", target, "Enter"]);
    if (await waitForDraftGone(target, fragment, 2_500)) return;
  }
  throw new WedgedInputError(target, recoveries.length);
}

/** Short recognizable prefix of the draft's first line, used to tell "our
 *  text is still sitting in the input" apart from every other ❯-prefixed row
 *  the TUI can show (permission pickers, placeholders). Matching on OUR text
 *  matters: a naive "input row not empty → press Enter again" would auto-
 *  accept the default option of a permission dialog. */
function draftFragment(content: string): string {
  const first = content.split("\n").find((l) => l.trim().length > 0) ?? "";
  return first.trim().slice(0, 24);
}

/** Text after the LAST ❯ glyph on screen (trimmed), or null when no ❯ row is
 *  visible. The LAST one is the live input row — submitted messages re-render
 *  in the scrollback with a ❯ prefix too, so anything above it is history;
 *  treating history as "the draft is still there" false-fails after every
 *  successful submit and would re-paste duplicates via the recovery ladder. */
function lastPromptRow(pane: string): string | null {
  let row: string | null = null;
  for (const line of pane.split("\n")) {
    const t = line.trimStart();
    if (t.startsWith("❯")) row = t.slice(1).trim();
  }
  return row;
}

/** Poll until the LIVE INPUT ROW no longer carries the draft fragment —
 *  submit landed (or the TUI moved to turn view). False on deadline: the
 *  draft is still sitting unsent in the input. */
async function waitForDraftGone(
  target: string,
  fragment: string,
  deadlineMs: number,
): Promise<boolean> {
  const start = Date.now();
  while (Date.now() - start < deadlineMs) {
    const { stdout } = await tmux(["capture-pane", "-t", target, "-p"]).catch(() => ({
      stdout: "",
      stderr: "",
    }));
    if (stdout) {
      const row = lastPromptRow(stdout);
      // Multiline pastes can render as a "[Pasted text #N +K lines]" chip
      // instead of the literal draft — we pasted into this input, so a
      // lingering chip is equally "our draft unsent".
      const stuck =
        row !== null &&
        ((fragment.length > 0 && row.startsWith(fragment)) || row.startsWith("[Pasted text"));
      if (!stuck) return true;
    }
    await sleep(150);
  }
  return false;
}

/** Poll the pane until consecutive captures stay identical for `settleMs`,
 *  capped at `deadlineMs`. Best-effort — proceeds even on timeout. */
async function waitForInputSettled(
  target: string,
  settleMs: number,
  deadlineMs: number,
): Promise<void> {
  const start = Date.now();
  let last = "";
  let lastChange = Date.now();
  while (Date.now() - start < deadlineMs) {
    const { stdout } = await tmux(["capture-pane", "-t", target, "-p"]).catch(() => ({
      stdout: "",
      stderr: "",
    }));
    if (stdout !== last) {
      last = stdout;
      lastChange = Date.now();
    } else if (Date.now() - lastChange >= settleMs) {
      return;
    }
    await sleep(50);
  }
}

async function dismissTrustPrompt(target: string, timeoutMs: number): Promise<void> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    const { stdout } = await tmux(["capture-pane", "-t", target, "-p"]).catch(() => ({
      stdout: "",
      stderr: "",
    }));
    if (stdout.includes("trust this folder")) {
      await tmux(["send-keys", "-t", target, "Enter"]).catch(() => {});
      return;
    }
    if (stdout.includes("❯") && !stdout.includes("trust")) return;
    await sleep(200);
  }
}

// Some pre-input screens look "ready" in the naive sense (they have the
// ❯ glyph) but actually want a numbered-option keypress before yielding
// the real input prompt. The big one in the wild: long-lived chat
// sessions launch into a "Resume from summary?" picker on `--resume`,
// where ❯ sits next to option 1 but neither "auto mode" nor "Try " is
// on screen — so the naive readiness check times out, the pool
// respawns, and the next window hits the same picker. Detect the
// picker, auto-dismiss with option 1 (the default, "Resume from
// summary"), and let the next poll see the real input row.
export async function waitForTuiReady(target: string, timeoutMs: number): Promise<void> {
  const start = Date.now();
  let resumeDismissAttempts = 0;
  const MAX_RESUME_DISMISSALS = 2;
  while (Date.now() - start < timeoutMs) {
    const { stdout } = await tmux(["capture-pane", "-t", target, "-p"]).catch(() => ({
      stdout: "",
      stderr: "",
    }));
    if (stdout.includes("❯") && (stdout.includes("auto mode") || stdout.includes("Try "))) {
      return;
    }
    if (stdout.includes("Resume from summary")) {
      if (resumeDismissAttempts >= MAX_RESUME_DISMISSALS) {
        throw new Error(
          `TUI blocked at interactive prompt: ResumeFromSummary (auto-dismiss failed after ${MAX_RESUME_DISMISSALS} attempts)`,
        );
      }
      resumeDismissAttempts += 1;
      await tmux(["send-keys", "-t", target, "1"]).catch(() => {});
      await tmux(["send-keys", "-t", target, "Enter"]).catch(() => {});
      await sleep(300);
      continue;
    }
    await sleep(200);
  }
  throw new Error(`TUI did not become ready within ${timeoutMs}ms`);
}

async function waitForAssistant(
  transcriptPath: string,
  fromOffset: number,
  maxWaitMs: number,
  quiescentMs: number,
  awaitTurnComplete: boolean,
): Promise<string> {
  const start = Date.now();
  let lastChange = Date.now();
  let lastSize = fromOffset;
  let buffer = "";
  let haveAssistant = false;

  while (Date.now() - start < maxWaitMs) {
    let size: number;
    try {
      const stat = await fs.stat(transcriptPath);
      size = stat.size;
    } catch {
      await sleep(200);
      continue;
    }
    if (size > lastSize) {
      const fd = await fs.open(transcriptPath, "r");
      try {
        const buf = Buffer.alloc(size - lastSize);
        await fd.read(buf, 0, buf.length, lastSize);
        buffer += buf.toString("utf-8");
        lastSize = size;
        lastChange = Date.now();
      } finally {
        await fd.close();
      }
      if (!haveAssistant) {
        haveAssistant = buffer.split("\n").some(lineIsAssistant);
      }
    }
    if (awaitTurnComplete) {
      // Definitive end-of-turn: once the model has emitted an assistant
      // message with stop_reason "end_turn" past our offset, the reply is
      // complete — return immediately, no quiescence wait. Text from a
      // tool_use-terminated message is mid-turn narration and is ignored.
      const finalText = extractLastAssistantText(buffer, true);
      if (finalText) return finalText;
    } else if (haveAssistant && Date.now() - lastChange > quiescentMs) {
      const text = extractLastAssistantText(buffer);
      if (text) return text;
    }
    await sleep(200);
  }
  throw new Error(`timed out after ${maxWaitMs}ms waiting for assistant response`);
}

function lineIsAssistant(line: string): boolean {
  const t = line.trim();
  if (!t) return false;
  try {
    const ev = JSON.parse(t);
    return ev?.type === "assistant";
  } catch {
    return false;
  }
}

/** Last assistant message's concatenated text. When `requireEndTurn` is
 *  set, only assistant messages whose `stop_reason` is "end_turn" count —
 *  text emitted before a tool call (stop_reason "tool_use") is mid-turn
 *  narration, not the final reply, and is skipped. */
export function extractLastAssistantText(buffer: string, requireEndTurn = false): string | null {
  let last: string | null = null;
  for (const raw of buffer.split("\n")) {
    const line = raw.trim();
    if (!line) continue;
    let ev: any;
    try {
      ev = JSON.parse(line);
    } catch {
      continue;
    }
    if (ev?.type !== "assistant") continue;
    if (requireEndTurn && ev?.message?.stop_reason !== "end_turn") continue;
    const content = ev?.message?.content;
    if (!Array.isArray(content)) continue;
    let text = "";
    for (const block of content) {
      if (block?.type === "text" && typeof block.text === "string") {
        text += block.text;
      }
    }
    const trimmed = text.trim();
    if (trimmed) last = trimmed;
  }
  return last;
}

function sanitizeWindowName(s: string): string {
  return s
    .toLowerCase()
    .replace(/[^a-z0-9-]/g, "-")
    .slice(0, 16);
}

function shellQuote(s: string): string {
  // Single-quote escape: it's → 'it'\''s'
  return `'${s.replace(/'/g, "'\\''")}'`;
}

function sleep(ms: number): Promise<void> {
  return new Promise((r) => setTimeout(r, ms));
}

// ---- daily rotation helpers (TS mirror of nucleus_core::claude_session) ----

export type TurnRole = "user" | "assistant";

export interface Turn {
  role: TurnRole;
  text: string;
}

export interface RotationStats {
  considered: number;
  rotated: number;
  skipped: number;
  failed: number;
}

const SUMMARY_PROMPT =
  "Summarize this conversation in 5-10 bullets for tomorrow's session. " +
  "Focus on ongoing tasks, decisions made, key facts about the user, " +
  "and anything a fresh assistant would need to know. " +
  "Reply with only the bullets.";

const SYSTEM_INJECTED_PREFIXES = [
  "<ide_opened_file>",
  "<ide_diagnostics>",
  "<system-reminder>",
  "<command-message>",
  "<command-name>",
  "<command-args>",
  "<local-command-",
];

function isSystemInjectedUserTurn(text: string): boolean {
  const t = text.trimStart();
  return SYSTEM_INJECTED_PREFIXES.some((p) => t.startsWith(p));
}

function stripDatePreamble(s: string): string {
  const TAG = "[context: today is ";
  if (s.startsWith(TAG)) {
    const idx = s.indexOf("]\n\n");
    if (idx >= 0) return s.slice(idx + 3);
  }
  return s;
}

/** Read the last `n` user/assistant text turns from a Claude transcript
 *  JSONL. TS mirror of `nucleus_core::claude_session::last_n_turns`.
 *  Same filters: drop tool_use/tool_result/thinking blocks, drop
 *  Claude-Code-injected `<…>` user turns, strip the date preamble. */
export function lastNTurns(transcriptPath: string, n: number): Turn[] {
  let raw: string;
  try {
    raw = readFileSync(transcriptPath, "utf8");
  } catch {
    return [];
  }
  const turns: Turn[] = [];
  for (const line of raw.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed) continue;
    let obj: any;
    try {
      obj = JSON.parse(trimmed);
    } catch {
      continue;
    }
    const kind = obj.type;
    let role: TurnRole;
    if (kind === "user") role = "user";
    else if (kind === "assistant") role = "assistant";
    else continue;
    const content = obj.message?.content;
    if (content === undefined || content === null) continue;
    const parts: string[] = [];
    if (typeof content === "string") {
      parts.push(content);
    } else if (Array.isArray(content)) {
      for (const item of content) {
        if (item && item.type === "text" && typeof item.text === "string") {
          parts.push(item.text);
        }
      }
    }
    if (parts.length === 0) continue;
    let text = parts.join("\n");
    if (role === "user" && isSystemInjectedUserTurn(text)) continue;
    text = stripDatePreamble(text).trim();
    if (!text) continue;
    turns.push({ role, text });
  }
  return turns.length > n ? turns.slice(turns.length - n) : turns;
}

/** Construct the first message a freshly-rotated session sees. TS mirror
 *  of `build_priming_preamble`. */
export function buildPrimingPreamble(summary: string, replay: Turn[]): string {
  const lines: string[] = [];
  lines.push("[Yesterday's session summary, for context]");
  lines.push(summary.trim());
  lines.push("");
  lines.push("[Recent conversation, replayed for continuity]");
  for (const turn of replay) {
    const label = turn.role === "user" ? "USER" : "ASSISTANT";
    lines.push(`${label}: ${turn.text.trim()}`);
    lines.push("");
  }
  lines.push(
    "[End of priming. The user has not sent a new message yet — " +
      "acknowledge briefly that you have the context and stand by.]",
  );
  return lines.join("\n");
}

/** Sleep until the next 04:00 in NUCLEUS_TZ (falling back to TZ, then
 *  UTC). Used by index.ts to gate the daily rotation tick.
 *
 *  Testing override: setting NUCLEUS_ROTATION_TEST_DELAY_SECONDS to a
 *  positive integer short-circuits the 4am math and sleeps that many
 *  seconds instead — lets us validate rotation end-to-end without
 *  waiting until 4am. Leave unset in production. */
export async function sleepUntilNext4am(): Promise<void> {
  const override = process.env.NUCLEUS_ROTATION_TEST_DELAY_SECONDS;
  if (override) {
    const secs = Number.parseInt(override, 10);
    if (Number.isFinite(secs) && secs > 0) {
      await sleep(secs * 1000);
      return;
    }
  }
  const delayMs = msUntilNext4am(new Date(), resolveTz());
  await sleep(delayMs);
}

export function resolveTz(): string {
  const cands = [process.env.NUCLEUS_TZ, process.env.TZ];
  for (const c of cands) {
    if (!c) continue;
    // Validate by attempting a formatter — invalid IANA name throws.
    try {
      new Intl.DateTimeFormat("en-US", { timeZone: c });
      return c;
    } catch {
      // try next
    }
  }
  return "UTC";
}

/** Milliseconds from `now` until the next 04:00 local time in `tz`.
 *  Mirror of `duration_until_next_4am`. */
export function msUntilNext4am(now: Date, tz: string): number {
  // Build "now" expressed in tz as the components we'd see on a wall
  // clock there. Intl.DateTimeFormat gives us those parts; we then build
  // a Date that points at "today 04:00 in tz" by formatting backwards
  // (Date.UTC computed via a probe).
  const fmt = new Intl.DateTimeFormat("en-CA", {
    timeZone: tz,
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
  const parts = Object.fromEntries(fmt.formatToParts(now).map((p) => [p.type, p.value]));
  const localYear = Number(parts.year);
  const localMonth = Number(parts.month);
  const localDay = Number(parts.day);
  const localHour = Number(parts.hour);
  const localMinute = Number(parts.minute);
  const localSecond = Number(parts.second);

  // What instant corresponds to (localYear-localMonth-localDay 04:00:00)
  // in `tz`? We compute the tz offset at "now", apply it to get a target
  // UTC instant, then refine once to handle the DST edge case where the
  // 04:00 boundary is on a different offset than `now`.
  const nowUtcMs = now.getTime();
  const nowAsLocalMs = Date.UTC(
    localYear,
    localMonth - 1,
    localDay,
    localHour,
    localMinute,
    localSecond,
  );
  const offsetMs = nowAsLocalMs - nowUtcMs;

  const target0400Ms =
    Date.UTC(localYear, localMonth - 1, localDay, 4, 0, 0) - offsetMs;

  let delta = target0400Ms - nowUtcMs;
  // If 04:00 today already passed (or is *now* — be strict about
  // "next"), advance one full day.
  if (delta <= 0) delta += 24 * 60 * 60 * 1000;
  return delta;
}
