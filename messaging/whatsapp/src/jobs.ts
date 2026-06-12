// Jobs ledger + one-shot job-session runner (ADR-013 recut).
//
// THE deferred-work primitive: a job = one row in memory/jobs.db + one
// one-shot Claude session in the nucleus-whatsapp-jobs tmux session. Three
// kinds today (act / enrich / vault-import); S12-class long tasks (course
// builds, canvas artifacts) add kinds, not schema.
//
// OWNERSHIP (ADR-020): jobs.db is whatsapp-family-owned (bot + CLIs).
// Dashboard reads, if ever added, go through core::db::open_read_only.
// Future cross-process producers insert status='queued' rows for the bot
// to claim — the queue-table-owned-by-reader pattern; sketched in ADR-013,
// not built in v1.
//
// RESTART SEMANTICS: JOBS_TMUX_SESSION is part of index.ts's boot wipe
// (ALL_TMUX_SESSIONS), so a bot restart kills any in-flight job window —
// "orphaned" therefore means DEAD by construction, never "maybe still
// running" (the 2026-06-11 orphan-window outage made the wipe a hard
// invariant). sweepOrphans() runs at boot; act/vault-import orphans get a
// DM note, enrich orphans are silent. Transcript re-attach is deliberately
// out of v1 (session_id is recorded for manual `claude --resume`
// forensics).

import { DatabaseSync } from "node:sqlite";
import { randomUUID } from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import type { Config } from "./config.js";
import { Session } from "./claude_session.js";

export const JOBS_TMUX_SESSION = "nucleus-whatsapp-jobs";

export type JobKind = "act" | "enrich" | "vault-import";
export type JobStatus = "running" | "done" | "failed" | "orphaned";

/** Per-kind ask ceilings. Overridable per call — S12 course-builds will
 *  pass 30-60min. */
export const JOB_MAX_WAIT_MS: Record<JobKind, number> = {
  act: 10 * 60_000,
  enrich: 5 * 60_000,
  "vault-import": 10 * 60_000,
};

export interface JobRow {
  id: string;
  kind: JobKind;
  docId: string | null;
  chatId: string;
  instruction: string;
  status: JobStatus;
  createdAt: string;
  promotedAt: string | null;
  finishedAt: string | null;
  sessionId: string | null;
  resultSummary: string | null;
  error: string | null;
}

interface RawJobRow {
  id: string;
  kind: string;
  doc_id: string | null;
  chat_id: string;
  instruction: string;
  status: string;
  created_at: string;
  promoted_at: string | null;
  finished_at: string | null;
  session_id: string | null;
  result_summary: string | null;
  error: string | null;
}

function toRow(r: RawJobRow): JobRow {
  return {
    id: r.id,
    kind: r.kind as JobKind,
    docId: r.doc_id,
    chatId: r.chat_id,
    instruction: r.instruction,
    status: r.status as JobStatus,
    createdAt: r.created_at,
    promotedAt: r.promoted_at,
    finishedAt: r.finished_at,
    sessionId: r.session_id,
    resultSummary: r.result_summary,
    error: r.error,
  };
}

export class JobStore {
  private db: DatabaseSync;

  constructor(opts: { dbPath: string }) {
    fs.mkdirSync(path.dirname(opts.dbPath), { recursive: true });
    this.db = new DatabaseSync(opts.dbPath);
    this.db.exec(`PRAGMA journal_mode = WAL;`);
    this.db.exec(`PRAGMA busy_timeout = 5000;`);
    this.db.exec(`
      CREATE TABLE IF NOT EXISTS jobs (
        id             TEXT PRIMARY KEY,
        kind           TEXT NOT NULL,
        doc_id         TEXT,
        chat_id        TEXT NOT NULL,
        instruction    TEXT NOT NULL,
        status         TEXT NOT NULL DEFAULT 'running',
        created_at     TEXT NOT NULL,
        promoted_at    TEXT,
        finished_at    TEXT,
        session_id     TEXT,
        tmux_window    TEXT,
        result_summary TEXT,
        error          TEXT
      );
      CREATE INDEX IF NOT EXISTS idx_jobs_status_created
        ON jobs(status, created_at DESC);
    `);
  }

  insert(i: { kind: JobKind; chatId: string; docId?: string; instruction: string }): string {
    const id = randomUUID();
    this.db
      .prepare(
        `INSERT INTO jobs (id, kind, doc_id, chat_id, instruction, status, created_at)
         VALUES (?, ?, ?, ?, ?, 'running', ?)`,
      )
      .run(id, i.kind, i.docId ?? null, i.chatId, i.instruction, new Date().toISOString());
    return id;
  }

  setSession(id: string, sessionId: string, tmuxWindow: string): void {
    this.db
      .prepare(`UPDATE jobs SET session_id = ?, tmux_window = ? WHERE id = ?`)
      .run(sessionId, tmuxWindow, id);
  }

  markPromoted(id: string): void {
    this.db
      .prepare(`UPDATE jobs SET promoted_at = ? WHERE id = ? AND promoted_at IS NULL`)
      .run(new Date().toISOString(), id);
  }

  markDone(id: string, resultSummary: string): void {
    this.db
      .prepare(
        `UPDATE jobs SET status = 'done', finished_at = ?, result_summary = ? WHERE id = ?`,
      )
      .run(new Date().toISOString(), resultSummary.slice(0, 4000), id);
  }

  markFailed(id: string, error: string): void {
    this.db
      .prepare(`UPDATE jobs SET status = 'failed', finished_at = ?, error = ? WHERE id = ?`)
      .run(new Date().toISOString(), error.slice(0, 1000), id);
  }

  /** Boot sweep: any row still 'running' belonged to a previous process —
   *  and the tmux wipe is about to kill its window, so it is dead. Marks
   *  orphaned and returns the swept rows (caller decides who gets a DM
   *  note). */
  sweepOrphans(): JobRow[] {
    const rows = (
      this.db
        .prepare(`SELECT * FROM jobs WHERE status = 'running'`)
        .all() as unknown as RawJobRow[]
    ).map(toRow);
    if (rows.length > 0) {
      this.db
        .prepare(
          `UPDATE jobs SET status = 'orphaned', finished_at = ?,
                  error = 'bot restarted while job was running'
            WHERE status = 'running'`,
        )
        .run(new Date().toISOString());
    }
    return rows;
  }

  recent(limit = 50): JobRow[] {
    return (
      this.db
        .prepare(`SELECT * FROM jobs ORDER BY created_at DESC LIMIT ?`)
        .all(limit) as unknown as RawJobRow[]
    ).map(toRow);
  }
}

export interface StartJobOpts {
  store: JobStore;
  config: Config;
  kind: JobKind;
  chatId: string;
  docId?: string;
  /** Human-readable ledger copy (≤ a sentence). */
  instruction: string;
  /** What the session is actually asked. */
  prompt: string;
  /** Tight per-kind job persona — code-owned, never the operator persona. */
  appendSystemPrompt?: string;
  allowedTools?: string[];
  addDirs?: string[];
  maxWaitMs?: number;
}

export interface JobOutcome {
  jobId: string;
  reply: string;
  elapsedMs: number;
}

/** Start a job: ledger row inserted synchronously (so the caller has the
 *  id before anything async), session work returned as a promise. The
 *  ledger is updated to done/failed when the promise settles — callers
 *  never touch markDone/markFailed themselves. */
export function startJob(o: StartJobOpts): { jobId: string; promise: Promise<JobOutcome> } {
  const jobId = o.store.insert({
    kind: o.kind,
    chatId: o.chatId,
    docId: o.docId,
    instruction: o.instruction,
  });
  const t0 = Date.now();
  const promise = (async (): Promise<JobOutcome> => {
    try {
      const session = await Session.spawn({
        workspaceRoot: o.config.workspaceRoot,
        appendSystemPrompt: o.appendSystemPrompt,
        permissionMode: o.config.permissionMode,
        disallowedTools: o.config.disallowedTools,
        allowedTools: o.allowedTools,
        addDirs: o.addDirs,
        tmuxSession: JOBS_TMUX_SESSION,
        windowName: `job-${jobId.slice(0, 8)}`,
        readyTimeoutMs: 60_000,
        agentLabel: "whatsapp", // run-log rows for free (ADR-016)
      });
      o.store.setSession(jobId, session.sessionId, session.tmuxTarget);
      try {
        const reply = await session.ask(o.prompt, {
          maxWaitMs: o.maxWaitMs ?? JOB_MAX_WAIT_MS[o.kind],
          // Non-negotiable for agentic jobs: quiescence alone tears the
          // session down mid-tool (the dsu-prep failure class).
          awaitTurnComplete: true,
        });
        o.store.markDone(jobId, reply);
        return { jobId, reply, elapsedMs: Date.now() - t0 };
      } finally {
        await session.close().catch(() => {});
      }
    } catch (e) {
      o.store.markFailed(jobId, (e as Error).message);
      throw e;
    }
  })();
  return { jobId, promise };
}

/** Timeout-promotion race: resolves {settled:true, value} if `p` settles
 *  within `ms`, else {settled:false} — WITHOUT consuming `p` (the caller
 *  attaches its own .then/.catch for the deferred path; rejections inside
 *  the window surface here as a rejection). Timer cleared on settle. */
export async function withQuickWindow<T>(
  p: Promise<T>,
  ms: number,
): Promise<{ settled: true; value: T } | { settled: false }> {
  let timer: NodeJS.Timeout;
  const timeout = new Promise<{ settled: false }>((resolve) => {
    timer = setTimeout(() => resolve({ settled: false }), ms);
  });
  // Derived branch for the race. If the timeout wins and `p` later
  // rejects, this DERIVED promise would reject unhandled (the caller only
  // handles the original `p`) — silence it in the finally; harmless when
  // it resolved.
  const settledBranch = p.then((value) => ({ settled: true as const, value }));
  try {
    return await Promise.race([settledBranch, timeout]);
  } finally {
    clearTimeout(timer!);
    settledBranch.catch(() => {});
  }
}
