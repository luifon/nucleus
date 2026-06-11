// Diary API — per-agent dated entries from `memory/diaries/`.
// Mirrors `nucleus-dashboard/api/src/handlers/diary.rs`. Conventions
// per ADR-004.
// Wire types are ts-rs-generated from the Rust structs (./generated/).

import { jsonGet, qs } from "./client";
import type { Agent as DiaryAgent } from "./generated/Agent";
import type { Entry as DiaryEntry } from "./generated/Entry";

export type { Agent as DiaryAgent } from "./generated/Agent";
export type { Entry as DiaryEntry } from "./generated/Entry";

export const listDiaryAgents = () => jsonGet<DiaryAgent[]>("/diary/api/agents");

export const listRecentDiary = (opts: { agent?: string; date?: string; limit?: number } = {}) =>
  jsonGet<DiaryEntry[]>(
    `/diary/api/recent${qs({ agent: opts.agent, date: opts.date, limit: opts.limit })}`,
  );

/** Raw markdown text of a single entry. Used for re-fetching if the
 *  /recent response truncated something. */
export const getDiaryEntry = (path: string) =>
  fetch(`/diary/api/entry${qs({ path })}`).then(async (r) => {
    if (!r.ok) throw new Error(`/diary/api/entry → ${r.status}`);
    return r.text();
  });
