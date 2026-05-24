// Typed fetch wrappers. One block per backend surface; grows as
// surfaces land.

async function jsonGet<T>(path: string): Promise<T> {
  const res = await fetch(path);
  if (!res.ok) throw new Error(`${path} → ${res.status}`);
  return res.json() as Promise<T>;
}

async function jsonPost<T, B>(path: string, body: B): Promise<T> {
  const res = await fetch(path, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!res.ok) throw new Error(`${path} → ${res.status}`);
  return res.json() as Promise<T>;
}

// ─── infra ──────────────────────────────────────────────────────────────────

export type Health = {
  status: string;
  service: string;
  version: string;
};

export const getHealth = () => jsonGet<Health>("/api/health");

// ─── news ───────────────────────────────────────────────────────────────────

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

function qs(opts: ListItemsOpts): string {
  const params = new URLSearchParams();
  if (opts.fetchDate) params.set("fetch_date", opts.fetchDate);
  if (opts.minScore !== undefined) params.set("min_score", String(opts.minScore));
  if (opts.limit !== undefined) params.set("limit", String(opts.limit));
  const s = params.toString();
  return s ? `?${s}` : "";
}

export const listNewsItems = (opts: ListItemsOpts = {}) =>
  jsonGet<NewsItem[]>(`/news/api/items${qs(opts)}`);

export const listNewsNotable = (opts: ListItemsOpts = {}) =>
  jsonGet<NewsItem[]>(`/news/api/items/notable${qs(opts)}`);

export const listNewsSources = () => jsonGet<NewsSource[]>("/news/api/sources");

export const listNewsRuns = () => jsonGet<NewsRun[]>("/news/api/runs");

export const voteOnNews = (itemId: string, vote: 1 | -1) =>
  jsonPost<{ ok: boolean; item_id: string; vote: number }, { item_id: string; vote: number }>(
    "/news/api/vote",
    { item_id: itemId, vote },
  );
