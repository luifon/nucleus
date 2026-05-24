// Run-log index for tmux+claude agents — TS counterpart of
// `nucleus_core::runlog` (see ADR-016). Each spawn appends a pointer row to
// `memory/logs/<agent>/runs.jsonl` referencing the Claude transcript (which
// survives the tmux window being killed); we never copy the transcript.

import { promises as fs, existsSync } from "node:fs";
import * as path from "node:path";

// Keep the most recent N runs per agent; older finalized rows are gc'd.
export const MAX_ROWS_PER_AGENT = 50;

export interface RunRow {
  run_id: string;
  agent: string;
  session_id: string;
  transcript_path: string;
  tmux_target: string;
  started_at: string;
  ended_at?: string | null;
  ok?: boolean | null;
}

export function indexPath(workspaceRoot: string, agent: string): string {
  return path.join(workspaceRoot, "memory", "logs", agent, "runs.jsonl");
}

/** Append an in-flight row at spawn. Best-effort — callers swallow errors. */
export async function recordStart(workspaceRoot: string, row: RunRow): Promise<void> {
  const p = indexPath(workspaceRoot, row.agent);
  await fs.mkdir(path.dirname(p), { recursive: true });
  await fs.appendFile(p, JSON.stringify(row) + "\n", "utf8");
}

/** Read all rows for an agent, oldest first. Missing file → empty. */
export async function read(workspaceRoot: string, agent: string): Promise<RunRow[]> {
  let content: string;
  try {
    content = await fs.readFile(indexPath(workspaceRoot, agent), "utf8");
  } catch {
    return [];
  }
  const rows: RunRow[] = [];
  for (const line of content.split("\n")) {
    const t = line.trim();
    if (!t) continue;
    try {
      rows.push(JSON.parse(t) as RunRow);
    } catch {
      /* skip malformed */
    }
  }
  return rows;
}

/** Finalize the row for `runId` (set ended_at/ok), then gc + cap. */
export async function recordEnd(
  workspaceRoot: string,
  agent: string,
  runId: string,
  ok: boolean,
): Promise<void> {
  const p = indexPath(workspaceRoot, agent);
  const rows = await read(workspaceRoot, agent);
  const r = rows.find((row) => row.run_id === runId);
  if (r) {
    r.ended_at = new Date().toISOString();
    r.ok = ok;
  }
  gc(rows);
  await writeAll(p, rows);
}

// Drop finalized rows whose transcript is gone (in-flight rows kept — the
// file appears just after spawn), then cap to the most recent MAX.
function gc(rows: RunRow[]): void {
  for (let i = rows.length - 1; i >= 0; i--) {
    const r = rows[i];
    if (r.ended_at != null && !existsSync(r.transcript_path)) rows.splice(i, 1);
  }
  if (rows.length > MAX_ROWS_PER_AGENT) {
    rows.splice(0, rows.length - MAX_ROWS_PER_AGENT);
  }
}

async function writeAll(p: string, rows: RunRow[]): Promise<void> {
  await fs.mkdir(path.dirname(p), { recursive: true });
  const buf = rows.map((r) => JSON.stringify(r)).join("\n") + (rows.length ? "\n" : "");
  const tmp = p + ".tmp";
  await fs.writeFile(tmp, buf, "utf8");
  await fs.rename(tmp, p); // atomic — a crashed write can't truncate the index
}
