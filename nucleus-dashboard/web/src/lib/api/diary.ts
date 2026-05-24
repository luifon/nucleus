// Diary API — per-agent dated entries from `memory/diaries/`.
// Mirrors `nucleus-dashboard/api/src/handlers/diary.rs`. Conventions
// per ADR-004.

import { jsonGet, qs } from "./client";

export type DiaryAgent = {
  name: string;
  entry_count: number;
  /** `YYYY-MM-DD` or null when the agent has no entries (shouldn't
   *  happen since we only list agents with at least one folder). */
  last_entry_date: string | null;
};

export type DiaryEntry = {
  agent: string;
  date: string;
  path: string;
  body: string;
  bytes: number;
};

export const listDiaryAgents = () => jsonGet<DiaryAgent[]>("/diary/api/agents");

export const listRecentDiary = (opts: { agent?: string; limit?: number } = {}) =>
  jsonGet<DiaryEntry[]>(
    `/diary/api/recent${qs({ agent: opts.agent, limit: opts.limit })}`,
  );

/** Raw markdown text of a single entry. Used for re-fetching if the
 *  /recent response truncated something. */
export const getDiaryEntry = (path: string) =>
  fetch(`/diary/api/entry${qs({ path })}`).then(async (r) => {
    if (!r.ok) throw new Error(`/diary/api/entry → ${r.status}`);
    return r.text();
  });
