// Daily compaction lock (ADR-025). Mirror of core/src/compaction_lock.rs.
//
// tools/restart-bots.sh restarts the resident bots at 04:00, the same minute
// the daily compaction starts, and a compaction takes minutes. While a bot
// compacts it holds `memory/compaction/<agent>.lock` (its pid and start
// time); the restart waits until no live lock is left. The lock is taken
// COMPACTION_LOCK_LEAD_MS before 04:00, so it exists when the restart job
// starts at 04:00. A lock whose pid is dead or that is older than
// COMPACTION_LOCK_MAX_AGE_SECS is stale and does not hold the restart.

import fs from "node:fs";
import path from "node:path";
import { msUntilNextCompaction } from "./claude_session.js";

/** Lock directory, relative to the workspace root. */
export const COMPACTION_LOCK_DIR = "memory/compaction";

/** The lock is taken this long before the compaction starts. */
export const COMPACTION_LOCK_LEAD_MS = 60_000;

/** A whole pass ends within this time: every step's timeout is cut to what
 *  is left of it, and a chat is not started once it is used up. */
export const DAILY_COMPACTION_BUDGET_MS = 25 * 60_000;

/** restart-bots.sh treats an older lock as stale (and waits at most this
 *  long). The budget caps when a step starts and its timeout, but a step
 *  can run past the budget: the summary turn is retried once after an
 *  infrastructure error with the same timeout, and settling, typing and a
 *  resume each take up to a minute. Lead 1 + budget 25 + one summary retry
 *  5 + about 4 minutes of settling stays under 45. */
export const COMPACTION_LOCK_MAX_AGE_SECS = 45 * 60;

export interface CompactionLock {
  readonly path: string;
  /** Remove the lock file, when it is still this process's. */
  release(): void;
}

/** The text of a lock file: `pid=<pid>` and `started=<unix seconds>`. */
export function lockBody(pid: number, startedSecs: number): string {
  return `pid=${pid}\nstarted=${startedSecs}\n`;
}

/** Write `<root>/memory/compaction/<agent>.lock` for this process (replacing
 *  a stale one) and return it. */
export function takeCompactionLock(workspaceRoot: string, agent: string, nowMs = Date.now()): CompactionLock {
  const dir = path.join(workspaceRoot, COMPACTION_LOCK_DIR);
  fs.mkdirSync(dir, { recursive: true });
  const file = path.join(dir, `${agent}.lock`);
  const tmp = `${file}.${process.pid}.tmp`;
  const body = lockBody(process.pid, Math.floor(nowMs / 1000));
  fs.writeFileSync(tmp, body);
  fs.renameSync(tmp, file);
  return {
    path: file,
    release() {
      try {
        if (fs.readFileSync(file, "utf8") === body) fs.unlinkSync(file);
      } catch {
        // already gone
      }
    },
  };
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, Math.max(0, ms)));

/** Sleep until COMPACTION_LOCK_LEAD_MS before the next daily compaction,
 *  take the lock, sleep until the compaction time, and return the lock. The
 *  caller compacts and then releases it. A lock that cannot be written is
 *  reported to `onLockError` and the compaction runs without it (null). */
export async function waitForDailyCompaction(
  workspaceRoot: string,
  agent: string,
  onLockError: (e: Error) => void,
): Promise<CompactionLock | null> {
  const at = Date.now() + msUntilNextCompaction();
  await sleep(at - COMPACTION_LOCK_LEAD_MS - Date.now());
  let lock: CompactionLock | null = null;
  try {
    lock = takeCompactionLock(workspaceRoot, agent);
  } catch (e) {
    onLockError(e as Error);
  }
  await sleep(at - Date.now());
  return lock;
}
