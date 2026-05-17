// Long-lived interactive `claude` sessions driven via tmux.
//
// TS counterpart of `nucleus_core::claude_session` — same architecture:
// spawn `claude` in a tmux window, send messages via paste-buffer,
// tail the session transcript JSONL for assistant turns. No TUI scraping.

import { spawn, exec } from "node:child_process";
import { promisify } from "node:util";
import { promises as fs } from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { randomUUID } from "node:crypto";

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
}

export interface AskOptions {
  maxWaitMs?: number;
  /** "No new transcript lines for this long" → claude is done. */
  quiescentMs?: number;
}

export interface AskResult {
  reply: string;
  sessionId: string;
  elapsedMs: number;
  wasColdSpawn: boolean;
}

const DEFAULT_ASK: Required<AskOptions> = {
  maxWaitMs: 180_000,
  quiescentMs: 3_000,
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
  ) {}

  static async spawn(opts: SpawnOptions): Promise<Session> {
    const resuming = !!opts.resumeSessionId;
    const sessionId = opts.resumeSessionId ?? randomUUID();
    const windowName = opts.windowName ?? sessionId.slice(0, 8);

    const args: string[] = resuming
      ? ["--resume", sessionId]
      : ["--session-id", sessionId];
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

    await ensureTmuxSession(opts.tmuxSession);
    const target = `${opts.tmuxSession}:${windowName}`;
    await tmux(["new-window", "-t", opts.tmuxSession, "-n", windowName, inner]);

    await dismissTrustPrompt(target, 5_000);
    await waitForTuiReady(target, opts.readyTimeoutMs ?? 20_000);
    await sleep(500);

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
    return new Session(sessionId, target, transcriptPath, initialCursor);
  }

  async ask(message: string, opts: AskOptions = {}): Promise<string> {
    const ask = { ...DEFAULT_ASK, ...opts };
    const fromOffset = this.cursor;
    await pasteAndSend(this.tmuxTarget, withDatePreamble(message));
    const reply = await waitForAssistant(
      this.transcriptPath,
      fromOffset,
      ask.maxWaitMs,
      ask.quiescentMs,
    );
    try {
      const stat = await fs.stat(this.transcriptPath);
      this.cursor = stat.size;
    } catch {
      // best-effort; cursor stays
    }
    return reply;
  }

  async close(): Promise<void> {
    await tmux(["kill-window", "-t", this.tmuxTarget]).catch(() => {});
  }
}

/** Manages a Map<chatKey, Session>. One claude per chat, lazily spawned. */
export class SessionPool {
  private entries = new Map<string, { session: Session; lastActive: number; lock: Promise<void> }>();
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
    if (!entry) {
      wasColdSpawn = true;
      const session = await Session.spawn({
        workspaceRoot: this.config.workspaceRoot,
        appendSystemPrompt: this.config.appendSystemPrompt,
        permissionMode: this.config.permissionMode,
        disallowedTools: this.config.disallowedTools,
        addDirs: this.config.addDirs,
        tmuxSession: this.config.tmuxSession,
        windowName: sanitizeWindowName(chatKey),
        resumeSessionId,
      });
      entry = { session, lastActive: Date.now(), lock: Promise.resolve() };
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
      return {
        reply,
        sessionId: entry.session.sessionId,
        elapsedMs: Date.now() - t0,
        wasColdSpawn,
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
  addDirs?: string[];
  tmuxSession: string;
  idleTimeoutMs: number;
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

async function pasteAndSend(target: string, content: string): Promise<void> {
  // Load buffer from stdin so any content (quotes, emoji, newlines) is safe.
  await new Promise<void>((resolve, reject) => {
    const child = spawn("tmux", ["load-buffer", "-"], { stdio: ["pipe", "ignore", "pipe"] });
    let stderr = "";
    child.stderr.on("data", (d) => (stderr += d.toString()));
    child.on("error", reject);
    child.on("close", (code) =>
      code === 0 ? resolve() : reject(new Error(`load-buffer failed: ${stderr.trim()}`)),
    );
    child.stdin!.write(content);
    child.stdin!.end();
  });
  await tmux(["paste-buffer", "-t", target]);
  // Wait for the bracketed-paste sequence to fully drain into claude's TUI
  // before pressing Enter. Without this, large pastes leave the TUI in
  // mid-paste-mode when Enter arrives, so the Enter gets eaten as a literal
  // newline and the prompt sits queued unsent. Same fix as the Rust side
  // (core/src/claude_session.rs::wait_for_input_settled).
  await waitForInputSettled(target, 250, 10_000);
  await tmux(["send-keys", "-t", target, "Enter"]);
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

async function waitForTuiReady(target: string, timeoutMs: number): Promise<void> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    const { stdout } = await tmux(["capture-pane", "-t", target, "-p"]).catch(() => ({
      stdout: "",
      stderr: "",
    }));
    if (stdout.includes("❯") && (stdout.includes("auto mode") || stdout.includes("Try "))) {
      return;
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
    if (haveAssistant && Date.now() - lastChange > quiescentMs) {
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

function extractLastAssistantText(buffer: string): string | null {
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
