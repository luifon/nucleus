// News API — public read endpoints + vote.
// Mirrors `nucleus-dashboard/api/src/handlers/news.rs`. Routes live
// under `/news/api/*` because they stay publicly reachable through
// cloudflared after the ADR-011 Tailscale perimeter ships
// (everything else gets gated).
// Wire types are ts-rs-generated from the Rust structs (./generated/).

import { jsonGet, jsonPost, qs } from "./client";
import type { ItemDto as NewsItem } from "./generated/ItemDto";
import type { SourceDto as NewsSource } from "./generated/SourceDto";
import type { RunDto as NewsRun } from "./generated/RunDto";

export type { ItemDto as NewsItem } from "./generated/ItemDto";
export type { SourceDto as NewsSource } from "./generated/SourceDto";
export type { RunDto as NewsRun } from "./generated/RunDto";

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

export const voteOnNews = (itemId: string, vote: 1 | -1) =>
  jsonPost<{ ok: boolean; item_id: string; vote: number }, { item_id: string; vote: number }>(
    "/news/api/vote",
    { item_id: itemId, vote },
  );
