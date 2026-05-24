// News API — public read endpoints + vote.
// Mirrors `nucleus-dashboard/api/src/handlers/news.rs`. Routes live
// under `/news/api/*` because they stay publicly reachable through
// cloudflared after the ADR-011 Tailscale perimeter ships
// (everything else gets gated).

import { jsonGet, jsonPost, qs } from "./client";

export type NewsItem = {
  id: string;
  source_id: number;
  source_name: string;
  url: string;
  article_url: string | null;
  title: string;
  summary: string | null;
  published_at: string;
  published_date: string;
  fetch_date: string;
  notable_score: number | null;
  notable_reason: string | null;
  posted_to_discord: number;
  upvotes: number | null;
  downvotes: number | null;
};

export type NewsSource = {
  id: number;
  name: string;
  url: string;
  enabled: number;
  last_fetched_at: string | null;
  last_error: string | null;
};

export type NewsRun = {
  run_id: string;
  started_at: string;
  finished_at: string | null;
  items_new: number;
  items_notable: number;
  ok: number;
};

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
