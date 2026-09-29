// News API — read endpoints + vote.
// Mirrors `nucleus-dashboard/api/src/handlers/news.rs`. Routes live
// under `/news/api/*`. Behind the tailnet post-ADR-011 (not public).
// Wire types are ts-rs-generated from the Rust structs (./generated/).

import { jsonGet, jsonPost, qs } from "./client";
import type { ItemDto as NewsItem } from "./generated/ItemDto";
import type { SourceDto as NewsSource } from "./generated/SourceDto";
import type { RunDto as NewsRun } from "./generated/RunDto";
import type { BriefDto as NewsBrief } from "./generated/BriefDto";
import type { VoteReq } from "./generated/VoteReq";
import type { OpenReq } from "./generated/OpenReq";

export type { ItemDto as NewsItem } from "./generated/ItemDto";
export type { SourceDto as NewsSource } from "./generated/SourceDto";
export type { RunDto as NewsRun } from "./generated/RunDto";
export type { BriefDto as NewsBrief } from "./generated/BriefDto";

export type ListItemsOpts = {
  fetchDate?: string;
  minScore?: number;
  limit?: number;
};

export const listNewsItems = (opts: ListItemsOpts = {}) =>
  jsonGet<NewsItem[]>(
    `/news/api/items${qs({
      fetch_date: opts.fetchDate,
      min_score: opts.minScore,
      limit: opts.limit,
    })}`,
  );

export const listNewsNotable = (opts: ListItemsOpts = {}) =>
  jsonGet<NewsItem[]>(
    `/news/api/items/notable${qs({
      fetch_date: opts.fetchDate,
      min_score: opts.minScore,
      limit: opts.limit,
    })}`,
  );

export const listNewsSources = () => jsonGet<NewsSource[]>("/news/api/sources");

export const listNewsRuns = () => jsonGet<NewsRun[]>("/news/api/runs");

export const getNewsBrief = (fetchDate?: string) =>
  jsonGet<NewsBrief | null>(`/news/api/brief${qs({ fetch_date: fetchDate })}`);

// 0 clears a vote — the fetcher reads the latest row per item as the
// effective verdict (ADR-031), so taking one back is itself a vote. A reason
// goes only with a downvote, and is sent as a second downvote carrying it.
export const voteOnNews = (
  itemId: string,
  vote: 1 | -1 | 0,
  reason?: { key: string; note?: string },
) =>
  jsonPost<{ ok: boolean; item_id: string; vote: number }, VoteReq>("/news/api/vote", {
    item_id: itemId,
    vote,
    ...(reason ? { reason: reason.key, note: reason.note } : {}),
  });

// Opens only mark an item as read (ADR-031); they never feed ranking.
export const recordNewsOpen = (itemId: string, url: string) =>
  jsonPost<{ ok: boolean; item_id: string }, OpenReq>("/news/api/open", { item_id: itemId, url });
