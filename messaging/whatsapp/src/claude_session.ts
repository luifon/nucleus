// Long-lived interactive `claude` sessions driven via tmux.
//
// TS counterpart of `nucleus_core::claude_session` — same architecture:
// spawn `claude` in a tmux window, type every message into the pane as
// keyboard input (ADR-033), tail the session transcript JSONL for assistant
// turns. No TUI scraping.

import { spawn, exec } from "node:child_process";
import { promisify } from "node:util";
import { promises as fs } from "node:fs";
import { readFileSync, statSync } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { randomUUID } from "node:crypto";

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
   *  `memory/logs/<agent>/runs.jsonl` pointing at the transcript, and the
   *  session runs with NUCLEUS_AGENT=<label> (the operator CLIs attribute
   *  its agent messages by it). */
  agentLabel?: string;
  /** Extra environment variables for the `claude` process and every tool
   *  command it runs (ADR-033: a DM chat session's NUCLEUS_TASK_SCOPE).
   *  Names must match [A-Z_][A-Z0-9_]*. */
  env?: Record<string, string>;
  /** Kind of session, set as NUCLEUS_SESSION in the `claude` start
   *  environment (ADR-033, proc_tree.ts): `chat`, `braindump`, `job`, or
   *  the default `agent`. The operator CLIs and the send scripts decide
   *  what a caller may do from it. */
  sessionKind?: string;
}

/** `env K='v' … ` for the launch command, or "" when there is nothing to
 *  set. Names are not quoted, so they are restricted; values are quoted.
 *  Mirror of core's env_prefix. Pure. */
export function envPrefix(env: Record<string, string>): string {
  const entries = Object.entries(env);
  if (entries.length === 0) return "";
  let out = "env ";
  for (const [k, v] of entries) {
    if (!/^[A-Z_][A-Z0-9_]*$/.test(k)) {
      throw new Error(`invalid environment variable name for a session: ${JSON.stringify(k)}`);
    }
    out += `${k}=${shellQuote(v)} `;
  }
  return out;
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
export function withDatePreamble(message: string): string {
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
    // Not readonly: `respawnOnFallback` relaunches the window and repoints
    // this at the new one.
    public tmuxTarget: string,
    public readonly transcriptPath: string,
    private cursor: number,
    // Run-log bookkeeping (ADR-016); set when spawned with an agentLabel.
    private readonly workspaceRoot: string = "",
    private readonly agentLabel?: string,
    private readonly runId: string = "",
    // The config this session was spawned with, kept so `ask` can relaunch
    // on the fallback model without the caller's help.
    private readonly spawnOpts?: SpawnOptions,
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
      opts,
    );
  }

  async ask(message: string, opts: AskOptions = {}): Promise<string> {
    checkPromptSize(message);
    const reply = await this.askOnce(message, opts);
    const kind = classifyInfraReply(reply);
    if (kind === null) return reply;
    // The session booted clean and died at INFERENCE time, so launchWindow's
    // boot-pane check saw nothing wrong and the banner arrived as the turn's
    // reply. Callers then posted it as content: on 2026-08-24 one fire sent
    // "issue with the selected model (claude-fable-5)" and another sent an
    // ENOTFOUND, each in place of the report it owed the operator.
    //
    // Fix what can be fixed, re-ask once, and never return the banner.
    const what = INFRA_ERROR_DESCRIPTION[kind];
    if (kind === "not-logged-in") {
      // Retrying can't mint credentials. Fail now so the caller alerts
      // instead of burning a second turn.
      throw new Error(`ask: ${what} — no reply was produced (log the CLI back in)`);
    }
    if (kind === "usage-limit" || kind === "no-turn") {
      // Neither recovers on a re-ask this turn: the quota only refills with
      // time, and the phantom means the session couldn't run the prompt at
      // all. Fail so the caller surfaces it, never the banner/phantom.
      throw new Error(`ask: ${what} — no reply was produced`);
    }
    if (kind === "model-unavailable") {
      const fb = fallbackModel();
      console.error(`whatsapp: ${what} — relaunching on fallback ${fb} and re-asking once`);
      await this.respawnOnFallback(fb);
    } else {
      // Transient. The window is fine — wait for the far side to recover and
      // re-ask in place.
      console.error(`whatsapp: ${what} — waiting ${API_ERROR_RETRY_DELAY_MS}ms and re-asking once`);
      await new Promise((r) => setTimeout(r, API_ERROR_RETRY_DELAY_MS));
    }
    const retry = await this.askOnce(message, opts);
    const again = classifyInfraReply(retry);
    if (again !== null) {
      // Never return the banner as content. A thrown error routes the caller
      // into its ⚠️ alert path, which says something useful.
      throw new Error(
        `ask: ${INFRA_ERROR_DESCRIPTION[again]} — still failing after one retry, so this turn ` +
          `produced no content`,
      );
    }
    return retry;
  }

  /** Kill this session's window and relaunch it on `model`, resuming the same
   *  session id so the chat keeps its history. Only `ask`'s model-failure
   *  path calls this. Mirrors core's respawn_on_fallback. */
  async respawnOnFallback(model: string): Promise<void> {
    if (!this.spawnOpts) {
      throw new Error("respawn: session has no spawn options (constructed directly?)");
    }
    await tmux(["kill-window", "-t", this.tmuxTarget]).catch(() => {});
    const windowName = this.spawnOpts.windowName ?? this.sessionId.slice(0, 8);
    // --resume on the same id: the transcript already holds the
    // conversation, including the turn that just failed.
    const target = await launchWindow(
      this.spawnOpts,
      this.sessionId,
      true,
      windowName,
      model,
    );
    if (target === null) {
      throw new Error(`respawn: fallback model ${model} is unavailable at boot too`);
    }
    this.tmuxTarget = target;
    // Same reason as spawn's resuming branch — pin past the existing
    // transcript so the retry can't read the failed turn back as its reply.
    try {
      const stat = await fs.stat(this.transcriptPath);
      this.cursor = stat.size;
    } catch {
      // best-effort; cursor stays
    }
  }

  /** One send-and-wait round trip. `ask` wraps this with the model-failure
   *  retry; nothing else should call it. */
  private async askOnce(message: string, opts: AskOptions = {}): Promise<string> {
    const ask = { ...DEFAULT_ASK, ...opts };
    // Settle before snapshotting the cursor. Anything written between spawn
    // (or the previous turn) and now is not an answer to THIS message.
    const settled = await waitForTranscriptQuiet(this.transcriptPath, ask.quiescentMs);
    if (settled !== null && settled > this.cursor) this.cursor = settled;
    const fromOffset = this.cursor;
    try {
      await submitInput(this.tmuxTarget, withDatePreamble(message), {
        transcriptPath: this.transcriptPath,
      });
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

  /** Type `payload` into the session and submit it, verified against the
   *  transcript, without waiting for a reply. The chat engine (ADR-033)
   *  follows the transcript itself. `marker` is a string unique to this
   *  payload; the submit counts as landed only when a transcript record
   *  carries it. Kills the window on a wedged input, like `ask`. Refuses a
   *  payload over MAX_TYPED_INPUT_BYTES before typing. Returns how the
   *  harness recorded the input (a pasted prompt is logged by submitInput;
   *  the caller records it). */
  async submit(payload: string, opts: { marker: string }): Promise<SubmitResult> {
    try {
      return await submitInput(this.tmuxTarget, payload, {
        transcriptPath: this.transcriptPath,
        marker: opts.marker,
      });
    } catch (e) {
      if (e instanceof WedgedInputError) await this.close().catch(() => {});
      throw e;
    }
  }

  /** Send one key to the window (the engine interrupts a turn that passed
   *  its safety ceiling with Escape). */
  async sendKey(key: string): Promise<void> {
    await tmux(["send-keys", "-t", this.tmuxTarget, key]);
  }

  /** Current pane text (the engine checks it for a permission prompt). */
  async capturePane(): Promise<string> {
    const { stdout } = await tmux(["capture-pane", "-t", this.tmuxTarget, "-p"]).catch(() => ({
      stdout: "",
      stderr: "",
    }));
    return stdout;
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

// ---- internals ----

/** Claude Code's directory name for a working directory: every character
 *  that is not an ASCII letter or digit becomes `-` (mirrors
 *  `project_dir_name` in core/src/claude_session.rs). */
export function projectDirName(dir: string): string {
  return dir.replace(/[^A-Za-z0-9]/g, "-");
}

function transcriptPathFor(workspaceRoot: string, sessionId: string): string {
  const encoded = projectDirName(workspaceRoot);
  return path.join(os.homedir(), ".claude", "projects", encoded, `${sessionId}.jsonl`);
}

/** The variables that identify a Nucleus session (core proc_tree::SESSION_VARS). */
export const SESSION_VARS = [
  "NUCLEUS_SESSION",
  "NUCLEUS_AGENT",
  "NUCLEUS_TASK_SCOPE",
  "NUCLEUS_TASK_WORKER",
  "CLAUDE_CODE_SESSION_ID",
];

async function ensureTmuxSession(name: string): Promise<void> {
  try {
    await execAsync(`tmux has-session -t ${shellQuote(name)}`);
    return;
  } catch {
    // not there
  }
  try {
    // The server a new session may start inherits this environment and
    // passes it to every window it opens; it must not carry a session's
    // identity (proc_tree.ts).
    const env = { ...process.env };
    for (const k of SESSION_VARS) delete env[k];
    await execAsync(`tmux new-session -d -s ${shellQuote(name)}`, { env });
  } catch (e) {
    // Two spawns that start together both see "no session" and both run
    // new-session; the second fails with "duplicate session" (seen with two
    // jobs at once, nucleus-whatsapp-jobs). The session exists either way.
    if (!String((e as Error).message).includes("duplicate session")) throw e;
  }
}

/** Fallback model for spawned sessions when the configured/default model is
 *  unavailable (fable-5 incident 2026-06-13). Mirrors core's fallback_model;
 *  default is the stable Opus the error banner itself recommends. */
export function fallbackModel(): string {
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

/** Upper bound on a reply that `classifyInfraReply` will judge. The banners
 *  are one or two sentences; a longer reply is a real answer that happens to
 *  discuss models or API errors, so it must not read as a failure. */
export const INFRA_ERROR_REPLY_MAX_LEN = 400;

/** How long to wait before re-asking after an API error. 529 Overloaded and
 *  the DNS failures clear on their own, but not instantly. */
const API_ERROR_RETRY_DELAY_MS = 10_000;

/** An `ask` reply that is infrastructure failing, not an answer. */
export type InfraError =
  | "model-unavailable"
  | "api"
  | "not-logged-in"
  | "usage-limit"
  | "no-turn";

const INFRA_ERROR_DESCRIPTION: Record<InfraError, string> = {
  "model-unavailable": "the model cannot serve inference",
  api: "the API is unreachable or overloaded",
  "not-logged-in": "the claude CLI is not logged in",
  "usage-limit": "the account hit its usage limit",
  "no-turn": "the session produced no real reply",
};

/** True if `reply` is the CLI's one-line usage/session-limit banner (Max
 *  subscription cap) rather than an answer. Requires a limit noun AND a
 *  banner verb on a single line, so a multi-line report mentioning a limit
 *  keeps its newlines and still delivers. Mirrors core's
 *  is_usage_limit_banner. */
function isUsageLimitBanner(reply: string): boolean {
  if (reply.includes("\n")) return false;
  const lc = reply.toLowerCase();
  return (
    (lc.includes("usage limit") || lc.includes("session limit")) &&
    (lc.includes("hit your") || lc.includes("reached") || lc.includes("resets"))
  );
}

/** True if `reply` is the "No response requested." phantom — a session that
 *  yielded no genuine assistant turn. Single line, exact modulo trailing
 *  punctuation. Mirrors core's is_no_turn_phantom. */
function isNoTurnPhantom(reply: string): boolean {
  if (reply.includes("\n")) return false;
  return reply.replace(/[.\s]+$/, "").toLowerCase() === "no response requested";
}

/** Classify an `ask` reply that is infrastructure failing rather than an
 *  answer, or null when it's real content.
 *
 *  The spawn-time pane check can't catch any of these: the session boots into
 *  a normal TUI and only fails when it has to serve inference, so the banner
 *  arrives as the turn's "reply" and gets delivered as content. Three fires
 *  did exactly that on 2026-08-24 — a fable-5 model banner and an ENOTFOUND
 *  both reached the operator's WhatsApp in place of the report they owed.
 *
 *  Mirrors core's classify_infra_reply — deliberately narrow, so a session
 *  explaining the incident can't retry itself. */
export function classifyInfraReply(reply: string): InfraError | null {
  const trimmed = reply.trim();
  if (trimmed.length > INFRA_ERROR_REPLY_MAX_LEN) return null;
  // Order matters: an expired login is fatal, so it must not be mistaken for
  // a transient API error and retried.
  if (trimmed.includes("Please run /login")) return "not-logged-in";
  if (paneShowsModelError(trimmed)) return "model-unavailable";
  // Usage limit and the phantom turn are fatal-for-this-turn: no in-turn
  // retry recovers them, so classify them before the transient API case.
  if (isUsageLimitBanner(trimmed)) return "usage-limit";
  if (isNoTurnPhantom(trimmed)) return "no-turn";
  if (trimmed.includes("API Error")) return "api";
  return null;
}

/** Directory holding the operator-private skill tree
 *  (`<dir>/.claude/skills/<name>/`). Gitignored; Claude Code only loads
 *  skills from it when the dir is passed with `--add-dir`. */
export const PRIVATE_SKILLS_DIR = ".nucleus";

/** The `--add-dir` list for a spawn: the caller's `addDirs`, then
 *  `<workspaceRoot>/.nucleus` when that directory exists and is not already
 *  listed. Mirrors core's `build_claude_args`. */
export function addDirsWithPrivateSkills(
  workspaceRoot: string,
  addDirs: readonly string[] | undefined,
): string[] {
  const dirs = [...(addDirs ?? [])];
  const privateDir = path.join(workspaceRoot, PRIVATE_SKILLS_DIR);
  const resolved = path.resolve(privateDir);
  if (dirs.some((d) => path.resolve(d) === resolved)) return dirs;
  let isDir = false;
  try {
    isDir = statSync(privateDir).isDirectory();
  } catch {
    isDir = false;
  }
  if (isDir) dirs.push(privateDir);
  return dirs;
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
  for (const d of addDirsWithPrivateSkills(opts.workspaceRoot, opts.addDirs)) {
    args.push("--add-dir", d);
  }
  if (opts.disallowedTools?.length) {
    args.push("--disallowed-tools", opts.disallowedTools.join(" "));
  }
  if (opts.allowedTools?.length) {
    args.push("--allowed-tools", opts.allowedTools.join(" "));
  }

  const env: Record<string, string> = { ...(opts.env ?? {}) };
  if (opts.agentLabel && env.NUCLEUS_AGENT === undefined) env.NUCLEUS_AGENT = opts.agentLabel;
  // Every Nucleus session is marked in its `claude` start environment,
  // which its tool commands cannot change (proc_tree.ts).
  if (env.NUCLEUS_SESSION === undefined) env.NUCLEUS_SESSION = opts.sessionKind ?? "agent";
  const inner = `cd ${shellQuote(opts.workspaceRoot)} && ${envPrefix(env)}claude ${args.map(shellQuote).join(" ")}`;

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

/** Typed input (ADR-033), mirror of core's TYPE_CHUNK_BYTES: one
 *  `send-keys -H` call per chunk of up to 128 bytes, 10 ms apart. The TUI
 *  treats one read of more than about 800 bytes as a paste and drops the
 *  head of the text; chunks that arrive while the TUI is busy are read
 *  together, so larger chunks (512 bytes was measured) turn into a paste
 *  under load; typeWithFlowControl waits for each chunk's echo. A 64 KiB
 *  prompt takes about 16 s. */
export const TYPE_CHUNK_BYTES = 128;
const TYPE_CHUNK_PAUSE_MS = 10;
/** Largest prompt typed into a session (core MAX_TYPED_PROMPT_BYTES). */
export const MAX_TYPED_PROMPT_BYTES = 64 * 1024;

/** Largest input typed by any path: a prompt of up to
 *  MAX_TYPED_PROMPT_BYTES plus the date preamble and headers the code adds
 *  (core MAX_TYPED_INPUT_BYTES). submitInput refuses more. */
export const MAX_TYPED_INPUT_BYTES = MAX_TYPED_PROMPT_BYTES + 1024;

/** Refuse a prompt over MAX_TYPED_PROMPT_BYTES with a clear error. */
export function checkPromptSize(prompt: string): void {
  checkSize(prompt, MAX_TYPED_PROMPT_BYTES);
}

/** Refuse typed input over MAX_TYPED_INPUT_BYTES. */
export function checkInputSize(input: string): void {
  checkSize(input, MAX_TYPED_INPUT_BYTES);
}

function checkSize(text: string, limit: number): void {
  const n = Buffer.byteLength(text, "utf8");
  if (n > limit) {
    throw new Error(
      `prompt is ${n} bytes; the limit for a typed prompt is ${limit} bytes. ` +
        "Split the input, or write it to a file and name the file in the prompt",
    );
  }
}

/** Make text safe to type: CRLF/CR → LF, tab → two spaces (Tab is a TUI
 *  key), every other control character dropped (ESC would start an escape
 *  sequence; ESC during a turn interrupts it). Mirror of core's
 *  sanitize_for_typing. */
export function sanitizeForTyping(content: string): string {
  let out = "";
  for (const c of content.replace(/\r\n/g, "\n").replace(/\r/g, "\n")) {
    const code = c.codePointAt(0)!;
    if (c === "\n") out += c;
    else if (c === "\t") out += "  ";
    else if (code < 0x20 || code === 0x7f) continue;
    else out += c;
  }
  return out;
}

/** Split into chunks of at most `maxBytes` UTF-8 bytes, never inside a
 *  character. Mirror of core's type_chunks. */
export function typeChunks(content: string, maxBytes: number): string[] {
  const max = Math.max(4, maxBytes);
  const out: string[] = [];
  let cur = "";
  let curBytes = 0;
  for (const c of content) {
    const n = Buffer.byteLength(c, "utf8");
    if (curBytes + n > max) {
      out.push(cur);
      cur = "";
      curBytes = 0;
    }
    cur += c;
    curBytes += n;
  }
  if (cur) out.push(cur);
  return out;
}

/** The tmux argument lists that type `content` into `target`, one
 *  `send-keys -H` call per chunk; each chunk goes as raw UTF-8 bytes
 *  because `send-keys -l` drops a trailing `;`. Mirror of core's
 *  type_invocations. Pure. */
export function typeInvocations(target: string, content: string): string[][] {
  return typeChunks(content, TYPE_CHUNK_BYTES).map((chunk) => [
    "send-keys",
    "-t",
    target,
    "-H",
    ...[...Buffer.from(chunk, "utf8")].map((b) => b.toString(16).padStart(2, "0")),
  ]);
}

/** Flow control between typed chunks: after a chunk, wait until the pane
 *  shows the tail of the text typed so far, so the next chunk is sent only
 *  after the TUI has read the previous one. Chunks sent into a TUI that is
 *  not reading them accumulate, and one read of more than about 800 bytes
 *  becomes a paste. Mirror of core's TYPE_ECHO_*. */
export const TYPE_ECHO_POLL_MS = 20;
/** Longest wait for one chunk's echo. On this bound the next chunk is sent
 *  anyway and the stall is counted. */
export const TYPE_ECHO_WAIT_MS = 2_000;
/** After this many stalls in one prompt, the rest is typed without waiting
 *  (the pane does not show the typed text, for example because it turned
 *  into a paste), so a 64 KiB prompt cannot take minutes. */
export const TYPE_ECHO_MAX_STALLS = 3;
/** Code points of the typed text the pane must show. */
const TYPE_ECHO_TAIL_CHARS = 16;

/** What `typeWithFlowControl` needs from tmux (a test double implements it). */
export interface TypingIo {
  send(args: string[]): Promise<void>;
  capture(): Promise<string>;
  sleep(ms: number): Promise<void>;
  now(): number;
}

/** How a prompt was typed. */
export interface TypingStats {
  chunks: number;
  /** Chunks whose echo did not appear within TYPE_ECHO_WAIT_MS. */
  stalls: number;
}

/** The last TYPE_ECHO_TAIL_CHARS non-whitespace code points of `typed`,
 *  whitespace removed (the TUI wraps long lines). Empty when `typed` has
 *  none. Pure. */
export function echoTail(typed: string): string {
  const chars = Array.from(squashWs(typed));
  return chars.slice(Math.max(0, chars.length - TYPE_ECHO_TAIL_CHARS)).join("");
}

/** Type `content` into `target` chunk by chunk with flow control. */
export async function typeWithFlowControl(target: string, content: string, io: TypingIo): Promise<TypingStats> {
  const chunks = typeChunks(content, TYPE_CHUNK_BYTES);
  const stats: TypingStats = { chunks: chunks.length, stalls: 0 };
  let typed = "";
  for (let i = 0; i < chunks.length; i++) {
    const chunk = chunks[i];
    await io.send([
      "send-keys",
      "-t",
      target,
      "-H",
      ...[...Buffer.from(chunk, "utf8")].map((b) => b.toString(16).padStart(2, "0")),
    ]);
    typed += chunk;
    await io.sleep(TYPE_CHUNK_PAUSE_MS);
    // The last chunk needs no echo wait: the submit path waits for the
    // draft to be visible and settled.
    if (i === chunks.length - 1 || stats.stalls >= TYPE_ECHO_MAX_STALLS) continue;
    const want = echoTail(typed);
    if (!want) continue;
    const start = io.now();
    for (;;) {
      if (squashWs(await io.capture()).includes(want)) break;
      if (io.now() - start >= TYPE_ECHO_WAIT_MS) {
        stats.stalls += 1;
        break;
      }
      await io.sleep(TYPE_ECHO_POLL_MS);
    }
  }
  return stats;
}

function tmuxTypingIo(target: string): TypingIo {
  return {
    send: async (args) => {
      await tmux(args);
    },
    capture: async () =>
      (await tmux(["capture-pane", "-t", target, "-p"]).catch(() => ({ stdout: "", stderr: "" }))).stdout,
    sleep,
    now: () => Date.now(),
  };
}

/** Type `content` as keyboard input, without pressing Enter. A line feed
 *  inserts a newline in the input box; it does not submit. */
async function typeInto(target: string, content: string): Promise<TypingStats> {
  const stats = await typeWithFlowControl(target, content, tmuxTypingIo(target));
  if (stats.stalls > 0) {
    console.error(
      `whatsapp: typing into ${target}: ${stats.stalls} of ${stats.chunks} chunks were not echoed within ${TYPE_ECHO_WAIT_MS}ms`,
    );
  }
  return stats;
}

/** Remove a multi-line draft without ESC: C-u clears one line, BSpace joins
 *  it to the line above. */
async function clearDraft(target: string, lines: number): Promise<void> {
  for (let i = 0; i <= lines; i++) {
    await tmux(["send-keys", "-t", target, "C-u"]);
    await tmux(["send-keys", "-t", target, "BSpace"]);
  }
}

/** The pane is waiting for a keypress answer (a permission dialog, the
 *  trust prompt, the resume picker) instead of holding a draft. Typing into
 *  it could answer it; Enter picks the highlighted option. Keys on the live
 *  row (the last ❯ row): a numbered option there is a picker. Text that
 *  merely appears in the scrollback (the model asking "Do you want to…?")
 *  does not count. Pure. */
export function paneAwaitingChoice(pane: string): string | null {
  const lines = pane.split("\n");
  let idx = -1;
  for (let i = 0; i < lines.length; i++) if (lines[i].trimStart().startsWith("❯")) idx = i;
  if (idx < 0) return null;
  const live = lines[idx].trimStart().slice(1).trim();
  if (/^\d+\.\s/.test(live)) return `option picker (${live.slice(0, 40)})`;
  const near = lines.slice(Math.max(0, idx - 4), idx + 3).join("\n");
  for (const m of ["trust this folder", "Resume from summary"]) {
    if (near.includes(m)) return m;
  }
  return null;
}

/** Did the harness accept `marker` in the transcript text `appended` — as a
 *  prompt that started a turn, as input queued while the session was busy
 *  (`queue-operation` enqueue), or as input absorbed into the running turn
 *  (`queued_command` attachment)? Pure; whitespace-insensitive because the
 *  harness may rewrap text. Mirror of core's transcript_accepted_marker. */
export function transcriptAccepted(appended: string, marker: string): boolean {
  return transcriptAcceptance(appended, marker).accepted;
}

/** How the harness recorded an accepted prompt. `promptSource` is the
 *  `promptSource` of the user record that carries the marker ("typed" for
 *  typed input); "pasted" when that record has no such field and its text
 *  is wrapped in `<pasted_content`; null when the input was accepted as
 *  queued or absorbed input (those records carry no source) or not at all.
 *  Mirror of core's transcript_acceptance. Pure. */
export interface Acceptance {
  accepted: boolean;
  via: "prompt" | "queued" | "absorbed" | null;
  promptSource: string | null;
}

export function transcriptAcceptance(appended: string, marker: string): Acceptance {
  const none: Acceptance = { accepted: false, via: null, promptSource: null };
  const want = squashWs(marker);
  if (!want) return none;
  for (const raw of appended.split("\n")) {
    const line = raw.trim();
    if (!line) continue;
    let v: any;
    try {
      v = JSON.parse(line);
    } catch {
      continue;
    }
    let text: string | null = null;
    let via: Acceptance["via"] = null;
    if (v?.type === "user") {
      const c = v.message?.content;
      if (typeof c === "string") text = c;
      else if (Array.isArray(c)) {
        text = c
          .filter((b: any) => typeof b?.text === "string")
          .map((b: any) => b.text)
          .join("\n");
      }
      via = "prompt";
    } else if (v?.type === "queue-operation" && typeof v.content === "string") {
      text = v.content;
      via = "queued";
    } else if (v?.type === "attachment" && typeof v.attachment?.prompt === "string") {
      text = v.attachment.prompt;
      via = "absorbed";
    }
    if (text === null || !squashWs(text).includes(want)) continue;
    let promptSource: string | null = null;
    if (via === "prompt") {
      if (typeof v.promptSource === "string") promptSource = v.promptSource;
      else if (text.includes("<pasted_content")) promptSource = "pasted";
    }
    return { accepted: true, via, promptSource };
  }
  return none;
}

/** A prompt the harness recorded as something other than typed input. */
export function arrivedPasted(a: { promptSource: string | null }): boolean {
  return a.promptSource !== null && a.promptSource !== "typed";
}

async function fileSize(p: string): Promise<number> {
  try {
    return (await fs.stat(p)).size;
  } catch {
    return 0;
  }
}

async function readFrom(p: string, offset: number): Promise<string> {
  try {
    const fh = await fs.open(p, "r");
    try {
      const size = (await fh.stat()).size;
      if (size <= offset) return "";
      const buf = Buffer.alloc(size - offset);
      await fh.read(buf, 0, buf.length, offset);
      return buf.toString("utf8");
    } finally {
      await fh.close();
    }
  } catch {
    return "";
  }
}

/** How long to wait for proof that a submit landed. */
const SUBMIT_CONFIRM_MS = 6_000;
/** How long a pane showing a picker may hold up a submit before it fails. */
const CHOICE_WAIT_MS = 30_000;

/** Type `content` into the input box and submit it, verifying it landed.
 *
 *  Every message this process sends is typed in chunks, so it reaches the
 *  model as a typed prompt (ADR-033). A bracketed paste would reach it
 *  wrapped in `<pasted_content>`, which Claude Code treats as possibly not
 *  written by the user; attributed agent messages carry their own
 *  code-written envelope instead.
 *
 *  Verification: with a transcript, the submit counts as landed only when a
 *  record past the pre-submit size carries `marker` (default: the head of
 *  the content) as a prompt, a queued input or an absorbed input — so input
 *  typed while the session is busy counts once the harness queues it.
 *  Ladder: Enter; a bare second Enter; then, only while OUR draft is still
 *  visible, clear it and put it in again. Never Enter into a picker.
 *
 *  Throws WedgedInputError when every rung failed. */
export async function submitInput(
  target: string,
  raw: string,
  opts: { transcriptPath?: string; marker?: string } = {},
): Promise<SubmitResult> {
  const content = sanitizeForTyping(raw);
  // Every typed input is bounded, whichever path submits it (ask, the chat
  // engine's submit, a context message).
  checkInputSize(content);
  const { head, tail } = draftFragments(content);
  const marker = opts.marker ?? head;
  const lines = content.split("\n").length;

  // Never type into a picker: wait (bounded) for it to go away.
  const waitStart = Date.now();
  for (;;) {
    const { stdout } = await tmux(["capture-pane", "-t", target, "-p"]).catch(() => ({
      stdout: "",
      stderr: "",
    }));
    const choice = paneAwaitingChoice(stdout);
    if (!choice) break;
    if (Date.now() - waitStart > CHOICE_WAIT_MS) {
      throw new Error(`input blocked: the session is showing a prompt (${choice}) — not typing into it`);
    }
    await sleep(500);
  }

  let stalls = 0;
  const put = async () => {
    stalls += (await typeInto(target, content)).stalls;
    await waitForDraftVisible(target, head, tail, 30_000);
    await waitForInputSettled(target, 250, 10_000);
  };
  await put();

  for (let rung = 0; rung < 3; rung++) {
    if (rung === 2) {
      if (!(await draftPresent(target, head, tail))) throw new WedgedInputError(target, 2);
      await clearDraft(target, lines);
      await put();
    }
    const { stdout: pane } = await tmux(["capture-pane", "-t", target, "-p"]).catch(() => ({
      stdout: "",
      stderr: "",
    }));
    if (!draftStuck(pane, head, tail)) {
      const choice = paneAwaitingChoice(pane);
      if (choice) {
        throw new Error(`input blocked: a prompt appeared (${choice}) — refusing to press Enter into it`);
      }
    }
    const from = opts.transcriptPath ? await fileSize(opts.transcriptPath) : 0;
    await tmux(["send-keys", "-t", target, "Enter"]);
    let acceptance: Acceptance | null = null;
    if (opts.transcriptPath) {
      const start = Date.now();
      while (Date.now() - start < SUBMIT_CONFIRM_MS) {
        const a = transcriptAcceptance(await readFrom(opts.transcriptPath, from), marker);
        if (a.accepted) {
          acceptance = a;
          break;
        }
        await sleep(150);
      }
    } else if (await waitForDraftGone(target, head, tail, SUBMIT_CONFIRM_MS)) {
      acceptance = { accepted: true, via: null, promptSource: null };
    }
    if (acceptance) {
      const result: SubmitResult = { via: acceptance.via, promptSource: acceptance.promptSource, typingStalls: stalls };
      if (arrivedPasted(acceptance)) {
        console.error(
          `whatsapp: the prompt submitted to ${target} arrived as ${acceptance.promptSource} content, not typed input`,
        );
      }
      return result;
    }
  }
  throw new WedgedInputError(target, 3);
}

/** What a verified submit observed. */
export interface SubmitResult {
  /** How the harness accepted the input (null: no transcript to check). */
  via: Acceptance["via"];
  /** `promptSource` of the accepted prompt record; null when the input was
   *  queued or absorbed (the record of a queued input carries none). */
  promptSource: string | null;
  /** Typed chunks whose echo did not appear in time (typeWithFlowControl). */
  typingStalls: number;
}

/** Poll until OUR draft is visible in the live input region (the text
 *  arrived and Enter will mean something). Best-effort. */
async function waitForDraftVisible(
  target: string,
  head: string,
  tail: string,
  deadlineMs: number,
): Promise<void> {
  const start = Date.now();
  while (Date.now() - start < deadlineMs) {
    if (await draftPresent(target, head, tail)) return;
    await sleep(100);
  }
}

/** Short recognizable prefix of the draft's first line, used to tell "our
 *  text is still sitting in the input" apart from every other ❯-prefixed row
 *  the TUI can show (permission pickers, placeholders). Matching on OUR text
 *  matters: a naive "input row not empty → press Enter again" would auto-
 *  accept the default option of a permission dialog. */
export function draftFragment(content: string): string {
  return draftFragments(content).head;
}

/** Head and tail markers for the pasted content.
 *
 *  The input box is a few rows tall, so a long payload scrolls and only its
 *  TAIL stays on screen — the first line is never visible. Matching the head
 *  alone made the verifier read every tall paste as "submit landed", so a
 *  wedged session sat with the prompt unsent and `ask` waited out its full
 *  timeout with no signal (2026-08-28). Matching either end covers the short
 *  and the tall shape. Mirror of core's draft_fragments. */
/** Is OUR draft currently visible in the live input row? Mirror of core's
 *  draft_present; gates the destructive recovery rung. */
export async function draftPresent(
  target: string,
  head: string,
  tail: string,
): Promise<boolean> {
  const { stdout } = await tmux(["capture-pane", "-t", target, "-p"]).catch(() => ({
    stdout: "",
    stderr: "",
  }));
  return stdout ? draftStuck(stdout, head, tail) : false;
}

export function draftFragments(content: string): { head: string; tail: string } {
  const lines = content.split("\n").filter((l) => l.trim().length > 0);
  // Counted in code points (Array.from), like core's `chars()`: a UTF-16
  // slice can cut an emoji's surrogate pair in half, and a lone surrogate
  // never matches the captured pane.
  const head = lines.length ? Array.from(lines[0].trim()).slice(0, 24).join("") : "";
  // LAST 24 chars of the whole content, matching core's draft_fragments. The
  // first 24 chars of the last line is wrong: a long final line wraps and its
  // start scrolls out of the input box, which is the very bug this fixes.
  const chars = Array.from(content.trimEnd());
  const tail = chars.slice(Math.max(0, chars.length - 24)).join("");
  return { head, tail };
}

/** Text after the LAST ❯ glyph on screen (trimmed), or null when no ❯ row is
 *  visible. The LAST one is the live input row — submitted messages re-render
 *  in the scrollback with a ❯ prefix too, so anything above it is history;
 *  treating history as "the draft is still there" false-fails after every
 *  successful submit and would re-paste duplicates via the recovery ladder. */
export function lastPromptRow(pane: string): string | null {
  let row: string | null = null;
  for (const line of pane.split("\n")) {
    const t = line.trimStart();
    if (t.startsWith("❯")) row = t.slice(1).trim();
  }
  return row;
}

/** Pure predicate behind waitForDraftGone: does the pane's live input row
 *  still carry OUR draft? Multiline pastes can render as a
 *  "[Pasted text #N +K lines]" chip instead of the literal draft — we pasted
 *  into this input, so a lingering chip is equally "our draft unsent".
 *  Matching on our fragment (never mere non-emptiness) is what keeps the
 *  recovery ladder from pressing Enter into a permission picker. Mirror of
 *  core/src/claude_session.rs::draft_stuck; shared vectors in
 *  core/testdata/submit_verify_vectors.json. */
export function liveInputRegion(pane: string): string | null {
  const lines = pane.split("\n");
  let idx = -1;
  for (let i = 0; i < lines.length; i++) {
    if (lines[i].trimStart().startsWith("❯")) idx = i;
  }
  if (idx < 0) return null;
  const head = lines[idx].trimStart().slice(1).trim();
  return [head, ...lines.slice(idx + 1).map((l) => l.trim())].join("\n");
}

/** Drop every whitespace character. The TUI hard-wraps a long line across
 *  pane rows, so a literal substring of the payload does not survive
 *  capture-pane — the wrap inserts a newline and an indent in the middle of
 *  it. Comparing with whitespace removed makes the match immune to where the
 *  wrap lands. Mirror of core's squash_ws. */
export function squashWs(s: string): string {
  return s.replace(/\s+/g, "");
}

export function draftStuck(pane: string, head: string, tail: string): boolean {
  const region = liveInputRegion(pane);
  if (region === null) return false;
  if (region.includes("[Pasted text")) return true;
  const r = squashWs(region);
  const h = squashWs(head);
  const t = squashWs(tail);
  return (h.length > 0 && r.includes(h)) || (t.length > 0 && r.includes(t));
}

/** Poll until the LIVE INPUT ROW no longer carries the draft fragment —
 *  submit landed (or the TUI moved to turn view). False on deadline: the
 *  draft is still sitting unsent in the input. */
export async function waitForDraftGone(
  target: string,
  head: string,
  tail: string,
  deadlineMs: number,
): Promise<boolean> {
  const start = Date.now();
  while (Date.now() - start < deadlineMs) {
    const { stdout } = await tmux(["capture-pane", "-t", target, "-p"]).catch(() => ({
      stdout: "",
      stderr: "",
    }));
    if (stdout && !draftStuck(stdout, head, tail)) return true;
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
      // Claude Code 2.1.26x+ opens the picker with "❯ No, exit" highlighted;
      // a blind Enter exits claude. Move to "Yes" and press Enter only once
      // the screen SHOWS the highlight on "Yes": a Down sent while the
      // picker is still starting is dropped, and the Enter after it then
      // exits (seen 2026-09-24 with 2.1.282). Mirror of core.
      const row = stdout.split("\n").find((l) => l.trimStart().startsWith("❯"));
      if (row && row.includes("No, exit")) {
        await tmux(["send-keys", "-t", target, "Down"]).catch(() => {});
        await sleep(300);
        continue;
      }
      if (row && row.includes("Yes")) {
        await tmux(["send-keys", "-t", target, "Enter"]).catch(() => {});
        return;
      }
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
    // A cold claude can take longer than dismissTrustPrompt's window to show
    // the trust picker; answer it here too (Down onto "Yes", then Enter once
    // the highlight is visibly there).
    if (stdout.includes("trust this folder")) {
      const row = stdout.split("\n").find((l) => l.trimStart().startsWith("❯")) ?? "";
      if (row.includes("No, exit")) {
        await tmux(["send-keys", "-t", target, "Down"]).catch(() => {});
      } else if (row.includes("Yes")) {
        await tmux(["send-keys", "-t", target, "Enter"]).catch(() => {});
      }
      await sleep(300);
      continue;
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

/** Poll `path` until its size stops changing for `settleMs`, then return that
 *  size. Mirrors core's `wait_for_transcript_quiet`.
 *
 *  Why the cursor cannot be taken at spawn time: on `--resume`, Claude Code
 *  shows the "Resume from summary" picker and `waitForTuiReady` auto-answers
 *  it with option 1. Answering injects a `Continue from where you left off.`
 *  user turn and the model replies to THAT. Both land in the transcript after
 *  the spawn cursor, so `askOnce` returned the phantom reply as the answer to
 *  the operator's message. On 2026-09-01 an operator DM got back "No response
 *  requested."; the real answer was written 20s later and never delivered.
 */
async function waitForTranscriptQuiet(
  path: string,
  settleMs: number,
): Promise<number | null> {
  const MAX_WAIT_MS = 60_000;
  const start = Date.now();
  let lastSize: number;
  try {
    lastSize = (await fs.stat(path)).size;
  } catch {
    return null;
  }
  let lastChange = Date.now();
  for (;;) {
    let size: number;
    try {
      size = (await fs.stat(path)).size;
    } catch {
      return null;
    }
    if (size !== lastSize) {
      lastSize = size;
      lastChange = Date.now();
    } else if (Date.now() - lastChange >= settleMs) {
      return size;
    }
    if (Date.now() - start >= MAX_WAIT_MS) return lastSize;
    await sleep(100);
  }
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

/** Two labeled sections (ADR-025): SUMMARY feeds tomorrow's priming as
 *  before; DURABLE is the memory flush — observations worth keeping beyond
 *  tomorrow that never made it into the diary. Split by splitRotationReply;
 *  a reply that ignores the format degrades to summary-only (pre-ADR-025
 *  behavior). Mirror of core/src/claude_session.rs::SUMMARY_PROMPT. */
export const SUMMARY_PROMPT =
  "This session rotates now. Reply with exactly two sections and no other text:\n" +
  "SUMMARY:\n" +
  "5-10 bullets for tomorrow's session — ongoing tasks, decisions made, " +
  "key facts about the user, anything a fresh assistant would need to know.\n" +
  "DURABLE:\n" +
  "Bullets for observations worth keeping beyond tomorrow that are NOT yet " +
  "recorded anywhere: decisions, corrections, recurring user preferences, " +
  "unresolved threads. Write none if everything durable is already recorded.";

/** Split the rotation reply into summary + durable. The DURABLE header is
 *  matched line-anchored (last occurrence wins); a missing header, empty
 *  body, or literal "none" yields durable: null. Mirror of
 *  core/src/claude_session.rs::split_rotation_reply; shared vectors in
 *  core/testdata/rotation_reply_vectors.json. */
export function splitRotationReply(reply: string): { summary: string; durable: string | null } {
  const headerRest = (line: string, name: string): string | null => {
    const t = line.trim();
    if (t.length < name.length + 1) return null;
    if (t.slice(0, name.length).toUpperCase() !== name) return null;
    return t[name.length] === ":" ? t.slice(name.length + 1) : null;
  };
  const lines = reply.split("\n");
  let durableIdx = -1;
  for (let i = lines.length - 1; i >= 0; i--) {
    if (headerRest(lines[i], "DURABLE") !== null) {
      durableIdx = i;
      break;
    }
  }
  if (durableIdx < 0) return { summary: reply.trim(), durable: null };

  const durableParts = [headerRest(lines[durableIdx], "DURABLE") ?? "", ...lines.slice(durableIdx + 1)];
  let durable: string | null = durableParts.join("\n").trim();
  if (durable.replace(/\.+$/, "").toLowerCase() === "none" || durable === "") durable = null;

  const head = lines.slice(0, durableIdx);
  const summaryIdx = head.findIndex((l) => headerRest(l, "SUMMARY") !== null);
  const summary =
    summaryIdx >= 0
      ? [headerRest(head[summaryIdx], "SUMMARY") ?? "", ...head.slice(summaryIdx + 1)].join("\n").trim()
      : head.join("\n").trim();
  return { summary, durable };
}

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
