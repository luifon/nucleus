// Which Claude Code session, if any, a process runs inside (ADR-033).
// TypeScript counterpart of core/src/proc_tree.rs; both run the vectors in
// core/testdata/caller_origin_vectors.json.
//
// The scripts that can put a message on WhatsApp (send.ts, ack.ts,
// enqueue-media.ts) decide who may run them from where they run. The
// script's own environment is not evidence: a session's tool command can
// remove or change any variable (`env -u NUCLEUS_TASK_WORKER …`). The facts
// read here cannot be changed by the command:
//
// - The process tree. Every session Nucleus starts runs its `claude` with
//   NUCLEUS_SESSION=<kind> (plus NUCLEUS_AGENT / NUCLEUS_TASK_SCOPE /
//   NUCLEUS_TASK_WORKER) in the environment it was started with, which its
//   descendants cannot edit. The outermost ancestor that carries the marker
//   decides, `claude` or not (a tmux server a session started keeps it).
//   `claude` processes are recognized by fixed names, never by a variable
//   of the caller's environment.
// - The controlling terminal. A command that detached from its parent keeps
//   the tmux pane's terminal; a Nucleus `claude` on that terminal identifies
//   the session.
//
// A process with neither (a new session via setsid) is "detached"; a process
// tree that cannot be read is "unknown". The scripts refuse both. A process
// a session starts on a new terminal with the Nucleus variables removed
// cannot be told from the operator's terminal (ADR-033); the target policy
// (target_policy.ts) still keeps it to the operator's DM and the configured
// groups.
//
// The start environment is read with `ps -E` (macOS) or /proc (Linux). `ps`
// prints the arguments and then the environment, separated by spaces; the
// environment part is the output of `ps -E` minus the output without it,
// and only NUCLEUS_* entries are kept. Their values never contain spaces
// (kinds, agent labels, hex tokens, task ids).

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";

export interface ProcNode {
  pid: number;
  claude: boolean;
  /** NUCLEUS_* variables of the start environment; null = unreadable. */
  env: Record<string, string> | null;
  session_id?: string | null;
}

export interface Snapshot {
  tty: string | null;
  /** Nearest first, excluding the root process. */
  ancestors: ProcNode[];
  tty_peers?: ProcNode[];
  complete?: boolean;
}

export interface NucleusSession {
  pid: number;
  kind: string;
  agent: string | null;
  scope: string | null;
  worker: string | null;
  sessionId: string | null;
}

export type Origin =
  | { origin: "terminal" }
  | { origin: "operator-session"; sessionId: string | null }
  | { origin: "nucleus"; session: NucleusSession }
  | { origin: "detached" }
  | { origin: "unknown"; reason: string };

export const SESSION_WORKER = "worker";
export const SESSION_CHAT = "chat";

function nonempty(env: Record<string, string>, key: string): string | null {
  const v = env[key]?.trim();
  return v ? v : null;
}

function nucleusOf(n: ProcNode, env: Record<string, string>): NucleusSession | null {
  const kind = nonempty(env, "NUCLEUS_SESSION");
  if (!kind) return null;
  return {
    pid: n.pid,
    kind,
    agent: nonempty(env, "NUCLEUS_AGENT"),
    scope: nonempty(env, "NUCLEUS_TASK_SCOPE"),
    worker: nonempty(env, "NUCLEUS_TASK_WORKER"),
    sessionId: n.session_id ?? null,
  };
}

export function isWorker(n: NucleusSession): boolean {
  return n.kind === SESSION_WORKER || n.worker !== null;
}

/** Decide the origin from a snapshot. Pure; mirrors proc_tree::classify.
 *  The outermost ancestor whose start environment carries the Nucleus
 *  marker decides, `claude` or not; which ancestors count as `claude`
 *  matters only when none carries it. */
export function classify(s: Snapshot): Origin {
  if (s.complete === false) return { origin: "unknown", reason: "the process ancestry could not be read" };
  let markedAt = -1;
  let marked: NucleusSession | null = null;
  s.ancestors.forEach((n, i) => {
    const m = n.env ? nucleusOf(n, n.env) : null;
    if (m) {
      markedAt = i;
      marked = m;
    }
  });
  // A claude whose environment cannot be read, further out than any marked
  // ancestor, may itself be the outermost Nucleus session.
  const hidden = s.ancestors.slice(markedAt + 1).find((n) => n.claude && !n.env);
  if (hidden) {
    return { origin: "unknown", reason: `the environment of the claude process ${hidden.pid} could not be read` };
  }
  if (marked) return { origin: "nucleus", session: marked };
  const claudes = s.ancestors.filter((n) => n.claude);
  const top = claudes[claudes.length - 1];
  if (top) return { origin: "operator-session", sessionId: top.session_id ?? null };
  if (!s.tty) return { origin: "detached" };
  let found: NucleusSession | null = null;
  for (const peer of (s.tty_peers ?? []).filter((p) => p.claude)) {
    if (!peer.env) {
      return {
        origin: "unknown",
        reason: `the environment of the claude process ${peer.pid} on this terminal could not be read`,
      };
    }
    const n = nucleusOf(peer, peer.env);
    if (!n) continue;
    if (found && (found.kind !== n.kind || found.agent !== n.agent || found.scope !== n.scope || found.worker !== n.worker)) {
      return { origin: "unknown", reason: "more than one Nucleus session runs on this terminal" };
    }
    found = n;
  }
  return found ? { origin: "nucleus", session: found } : { origin: "terminal" };
}

function base(p: string): string {
  return path.basename(p.replace(/^-/, ""));
}

/** Mirrors proc_tree::is_claude_exec: fixed names only. The caller's
 *  environment (NUCLEUS_CLAUDE_BIN) is not read — a command could set it to
 *  the name of any process above it. */
export function isClaudeExec(argv0: string, execPath: string): boolean {
  return base(argv0) === "claude" || base(execPath) === "claude" || execPath.includes("/claude/versions/");
}

/** Mirrors proc_tree::session_id_from_args. */
export function sessionIdFromArgs(args: string[]): string | null {
  for (let i = 0; i < args.length; i++) {
    for (const flag of ["--session-id", "--resume"]) {
      if (args[i] === flag) {
        const v = args[i + 1];
        return v && !v.startsWith("-") ? v : null;
      }
      if (args[i].startsWith(`${flag}=`)) return args[i].slice(flag.length + 1) || null;
    }
  }
  return null;
}

interface Row {
  pid: number;
  ppid: number;
  tty: string | null;
  comm: string;
}

function ps(args: string[]): string | null {
  try {
    return execFileSync("ps", args, { encoding: "utf8", stdio: ["ignore", "pipe", "ignore"], maxBuffer: 64 * 1024 * 1024 });
  } catch {
    return null;
  }
}

function table(): Map<number, Row> | null {
  const out = ps(["-A", "-o", "pid=,ppid=,tty=,comm="]);
  if (out === null) return null;
  const rows = new Map<number, Row>();
  for (const line of out.split("\n")) {
    const m = /^\s*(\d+)\s+(\d+)\s+(\S+)\s+(.*)$/.exec(line);
    if (!m) continue;
    const tty = m[3] === "??" || m[3] === "?" || m[3] === "-" ? null : m[3];
    rows.set(Number(m[1]), { pid: Number(m[1]), ppid: Number(m[2]), tty, comm: m[4].trim() });
  }
  return rows;
}

function nucleusEnv(entries: string[]): Record<string, string> {
  const env: Record<string, string> = {};
  for (const e of entries) {
    const m = /^(NUCLEUS_[A-Z0-9_]*)=(.*)$/.exec(e);
    if (m) env[m[1]] = m[2];
  }
  return env;
}

interface StartInfo {
  env: Record<string, string> | null;
  args: string[];
}

/** `pid` → its `command` column, from one `ps` call over `pids`. */
function commands(pids: number[], withEnv: boolean): Map<number, string> | null {
  const out = ps([...(withEnv ? ["-E"] : []), "-ww", "-o", "pid=,command=", "-p", pids.join(",")]);
  if (out === null) return null;
  const m = new Map<number, string>();
  for (const line of out.split("\n")) {
    const r = /^\s*(\d+) (.*)$/.exec(line);
    if (r) m.set(Number(r[1]), r[2]);
  }
  return m;
}

/** Start environment (NUCLEUS_* only) and arguments of each of `pids`. */
function startInfos(pids: number[]): Map<number, StartInfo> {
  const res = new Map<number, StartInfo>();
  if (pids.length === 0) return res;
  if (fs.existsSync("/proc/self/environ")) {
    for (const pid of pids) {
      try {
        const env = fs.readFileSync(`/proc/${pid}/environ`, "utf8").split("\0").filter(Boolean);
        const args = fs.readFileSync(`/proc/${pid}/cmdline`, "utf8").split("\0").filter(Boolean);
        res.set(pid, { env: env.length > 0 ? nucleusEnv(env) : null, args });
      } catch {
        res.set(pid, { env: null, args: [] });
      }
    }
    return res;
  }
  const plain = commands(pids, false);
  const withEnv = commands(pids, true);
  for (const pid of pids) {
    const a = plain?.get(pid);
    const b = withEnv?.get(pid);
    if (a === undefined || b === undefined) {
      res.set(pid, { env: null, args: [] });
      continue;
    }
    const args = a.split(" ").filter(Boolean);
    // Every process starts with some environment (HOME, PATH); none shown
    // means it is hidden (macOS hides it for platform binaries and for
    // other users' processes).
    const entries = b.startsWith(a) ? b.slice(a.length).split(" ").filter(Boolean) : [];
    res.set(pid, { env: entries.length > 0 ? nucleusEnv(entries) : null, args });
  }
  return res;
}

function nodeOf(row: Row, info: StartInfo | undefined): ProcNode {
  const claude = isClaudeExec(row.comm, row.comm);
  const env = info?.env ?? null;
  return claude
    ? { pid: row.pid, claude, env, session_id: sessionIdFromArgs((info?.args ?? []).slice(1)) }
    : { pid: row.pid, claude, env };
}

/** Read the snapshot of the calling process. */
export function snapshot(pid: number = process.pid): Snapshot {
  const rows = table();
  const me = rows?.get(pid);
  if (!rows || !me) return { tty: null, ancestors: [], complete: false };
  const s: Snapshot = { tty: me.tty, ancestors: [], tty_peers: [], complete: true };
  const chain: Row[] = [];
  let cur = me.ppid;
  let guard = 0;
  while (cur > 1) {
    const row = rows.get(cur);
    if (!row || ++guard > 256) {
      s.complete = false;
      break;
    }
    chain.push(row);
    cur = row.ppid;
  }
  // Every ancestor's start environment: a marked one decides, claude or not.
  const infos = startInfos(chain.map((r) => r.pid));
  s.ancestors = chain.map((r) => nodeOf(r, infos.get(r.pid)));
  if (s.ancestors.some((n) => n.claude || (n.env && nonempty(n.env, "NUCLEUS_SESSION"))) || !s.tty) return s;
  const peers = [...rows.values()].filter((r) => r.pid !== pid && r.tty === s.tty && isClaudeExec(r.comm, r.comm));
  const peerInfos = startInfos(peers.map((r) => r.pid));
  s.tty_peers = peers.map((r) => nodeOf(r, peerInfos.get(r.pid)));
  return s;
}

export function origin(): Origin {
  return classify(snapshot());
}
