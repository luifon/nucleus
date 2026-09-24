// Pure helpers for the usage surface (ADR-034): number formatting, period
// deltas, gap-filled series, heatmap grid. No React here, so vitest covers it.

import type { UsageHeatCell, UsagePrice, UsageRefreshRun, UsageSeriesPoint, UsageTotals } from "@/lib/api/usage";

export type Metric = "cost" | "tokens";

/** 1234 → "1.2k", 5.6e6 → "5.6M", 2.1e9 → "2.10B". */
export function formatTokens(n: number): string {
  const a = Math.abs(n);
  if (a >= 1e9) return `${(n / 1e9).toFixed(2)}B`;
  if (a >= 1e6) return `${(n / 1e6).toFixed(1)}M`;
  if (a >= 1e3) return `${(n / 1e3).toFixed(1)}k`;
  return `${Math.round(n)}`;
}

/** Dollars with precision that fits the size: $0.042, $4.20, $1,234. */
export function formatUsd(n: number): string {
  const a = Math.abs(n);
  if (a === 0) return "$0";
  if (a < 1) return `$${n.toFixed(3)}`;
  if (a < 1000) return `$${n.toFixed(2)}`;
  return `$${Math.round(n).toLocaleString("en-US")}`;
}

export function formatMetric(metric: Metric, n: number): string {
  return metric === "cost" ? formatUsd(n) : formatTokens(n);
}

export function metricOf(metric: Metric, t: Pick<UsageTotals, "cost_usd" | "tokens">): number {
  return metric === "cost" ? t.cost_usd : t.tokens;
}

/** Signed relative change; null when the previous period is empty. */
export function deltaPct(current: number, previous: number): number | null {
  if (previous === 0) return null;
  return ((current - previous) / previous) * 100;
}

/** Share of input-side tokens served from cache. */
export function cacheHitRatio(t: Pick<UsageTotals, "input" | "cache_read" | "cache_write">): number | null {
  const denom = t.input + t.cache_read + t.cache_write;
  return denom === 0 ? null : t.cache_read / denom;
}

function addDays(day: string, n: number): string {
  const d = new Date(`${day}T00:00:00Z`);
  d.setUTCDate(d.getUTCDate() + n);
  return d.toISOString().slice(0, 10);
}

export type StackedBucket = { bucket: string; claude: number; codex: number };

/** Pivot per-vendor points into one row per bucket, filling missing days
 *  between `from` and `to` (inclusive) with zeros. `stepDays` 7 for weeks. */
export function pivotSeries(
  points: UsageSeriesPoint[],
  metric: Metric,
  from: string,
  to: string,
  stepDays = 1,
): StackedBucket[] {
  const byBucket = new Map<string, StackedBucket>();
  for (const p of points) {
    const row = byBucket.get(p.bucket) ?? { bucket: p.bucket, claude: 0, codex: 0 };
    const v = metric === "cost" ? p.cost_usd : p.tokens;
    if (p.vendor === "codex") row.codex += v;
    else row.claude += v;
    byBucket.set(p.bucket, row);
  }
  const out: StackedBucket[] = [];
  if (!from || from > to) return [...byBucket.values()].sort((a, b) => a.bucket.localeCompare(b.bucket));
  for (let d = from; d <= to; d = addDays(d, stepDays)) {
    out.push(byBucket.get(d) ?? { bucket: d, claude: 0, codex: 0 });
  }
  return out;
}

/** Monday of the ISO week containing `day`. */
export function weekStart(day: string): string {
  const d = new Date(`${day}T00:00:00Z`);
  const dow = (d.getUTCDay() + 6) % 7;
  return addDays(day, -dow);
}

export { addDays };

/** 7 × 24 grid (Monday first) of the chosen metric. */
export function heatGrid(cells: UsageHeatCell[], metric: Metric): number[][] {
  const grid = Array.from({ length: 7 }, () => Array<number>(24).fill(0));
  for (const c of cells) {
    if (c.dow < 0 || c.dow > 6 || c.hour < 0 || c.hour > 23) continue;
    grid[c.dow][c.hour] += metric === "cost" ? c.cost_usd : c.tokens;
  }
  return grid;
}

/** Five-step sequential class for a heat value (0 = empty). Quantile-free:
 *  steps are fractions of the max, so the scale legend can state them. */
export function heatStep(v: number, max: number): number {
  if (v <= 0 || max <= 0) return 0;
  const f = v / max;
  if (f > 0.8) return 5;
  if (f > 0.6) return 4;
  if (f > 0.4) return 3;
  if (f > 0.2) return 2;
  return 1;
}

/** "Nice" axis maximum and ticks for a value range starting at 0. */
export function niceTicks(max: number, count = 4): number[] {
  if (max <= 0) return [0];
  const raw = max / count;
  const mag = Math.pow(10, Math.floor(Math.log10(raw)));
  const norm = raw / mag;
  const step = (norm <= 1 ? 1 : norm <= 2 ? 2 : norm <= 2.5 ? 2.5 : norm <= 5 ? 5 : 10) * mag;
  const ticks: number[] = [];
  for (let v = 0; v <= max + step * 0.001; v += step) ticks.push(v);
  if (ticks[ticks.length - 1] < max) ticks.push(ticks[ticks.length - 1] + step);
  return ticks;
}

/** Axis tick label: whole numbers, compact. */
export function formatTick(metric: Metric, n: number): string {
  if (metric === "tokens") return formatTokens(n);
  if (n >= 1000) return `\$${(n / 1000).toFixed(n % 1000 === 0 ? 0 : 1)}k`;
  return Number.isInteger(n) ? `\$${n}` : `\$${n.toFixed(2)}`;
}

export type VendorChoice = "all" | "claude" | "codex";

/** Tool filter from the URL (`?tool=`); anything unknown means all. */
export function parseVendor(raw: string | null): VendorChoice {
  return raw === "claude" || raw === "codex" ? raw : "all";
}

/** Which tools a total covers, stated next to every headline number. */
export function scopeLabel(v: VendorChoice): string {
  return v === "all" ? "Claude + Codex" : v === "claude" ? "Claude" : "Codex";
}

/** Tokens of models without a price are counted but add nothing to the
 *  dollar figure; say so next to it instead of showing a lower number. */
export function unpricedNote(t: Pick<UsageTotals, "unpriced_tokens">): string | null {
  return t.unpriced_tokens > 0 ? `+ ${formatTokens(t.unpriced_tokens)} tokens without a price` : null;
}

/** Part of a dollar figure priced from a third-party estimate (a model
 *  whose vendor publishes no price) instead of an API list price. */
export function thirdPartyNote(t: Pick<UsageTotals, "third_party_usd">): string | null {
  return t.third_party_usd > 0 ? `incl. ${formatUsd(t.third_party_usd)} at a third-party estimate` : null;
}

/** Tooltip naming each third-party source: model, URL, retrieval date. */
export function thirdPartySources(prices: UsagePrice[]): string {
  return prices
    .filter((p) => p.basis === "third-party-estimate")
    .map((p) => `${p.model}: third-party estimate, not an API list price. Source ${p.source_url ?? "?"}, retrieved ${p.retrieved ?? "?"}`)
    .join("\n");
}

/** Label of a price's basis in the price table. */
export function basisLabel(basis: string | null): string {
  if (basis === "list-price") return "API list price";
  if (basis === "third-party-estimate") return "third-party estimate";
  if (basis === "nucleus.toml") return "nucleus.toml";
  return "—";
}

/** Whether a range needs the "Claude data before X is incomplete" note.
 *  All time always reaches back to before the first refresh, so the note
 *  shows whenever Claude is in scope; a bounded range shows it when its
 *  first day (or its comparison period) starts before `completeSince`. */
export function claudeRetentionWarning(opts: {
  vendor: VendorChoice;
  days: number;
  from: (string | null | undefined)[];
  completeSince: string | null;
}): boolean {
  if (opts.vendor === "codex" || !opts.completeSince) return false;
  if (opts.days === 0) return true;
  return opts.from.some((x) => !!x && x < opts.completeSince!);
}

/** Every local day from `from` to `to`, inclusive (server-computed bounds). */
export function dayRange(from: string, to: string): string[] {
  const out: string[] = [];
  if (!from || !to || from > to) return out;
  for (let d = from; d <= to; d = addDays(d, 1)) out.push(d);
  return out;
}

/** A finished refresh that left data out; the text for the page, or null. */
export function partialRefreshNote(run: Pick<UsageRefreshRun, "files_failed" | "malformed_lines" | "oversized_lines"> | null): string | null {
  if (!run) return null;
  const parts: string[] = [];
  if (run.files_failed > 0) parts.push(`${run.files_failed} file${run.files_failed > 1 ? "s" : ""} not read`);
  if (run.malformed_lines > 0) parts.push(`${run.malformed_lines} malformed usage line${run.malformed_lines > 1 ? "s" : ""}`);
  if (run.oversized_lines > 0) parts.push(`${run.oversized_lines} oversized line${run.oversized_lines > 1 ? "s" : ""}`);
  return parts.length ? parts.join(", ") : null;
}
