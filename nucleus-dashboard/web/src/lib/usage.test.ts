import { describe, expect, it } from "vitest";
import {
  basisLabel,
  cacheHitRatio,
  claudeRetentionWarning,
  dayRange,
  partialRefreshNote,
  thirdPartyNote,
  thirdPartySources,
  deltaPct,
  formatTokens,
  formatTick,
  formatUsd,
  heatGrid,
  heatStep,
  niceTicks,
  parseVendor,
  pivotSeries,
  scopeLabel,
  unpricedNote,
  weekStart,
} from "./usage";

describe("tool filter and unpriced display", () => {
  it("parses the URL value", () => {
    expect(parseVendor("codex")).toBe("codex");
    expect(parseVendor("claude")).toBe("claude");
    expect(parseVendor(null)).toBe("all");
    expect(parseVendor("bogus")).toBe("all");
  });
  it("labels the scope of a total", () => {
    expect(scopeLabel("all")).toBe("Claude + Codex");
    expect(scopeLabel("codex")).toBe("Codex");
  });
  it("states unpriced tokens next to a dollar figure", () => {
    expect(unpricedNote({ unpriced_tokens: 0 })).toBeNull();
    expect(unpricedNote({ unpriced_tokens: 788_935_031 })).toBe("+ 788.9M tokens without a price");
  });
});

describe("formatting", () => {
  it("compacts tokens", () => {
    expect(formatTokens(999)).toBe("999");
    expect(formatTokens(1234)).toBe("1.2k");
    expect(formatTokens(5_600_000)).toBe("5.6M");
    expect(formatTokens(2_100_000_000)).toBe("2.10B");
  });
  it("sizes dollar precision", () => {
    expect(formatUsd(0)).toBe("$0");
    expect(formatUsd(0.0421)).toBe("$0.042");
    expect(formatUsd(4.2)).toBe("$4.20");
    expect(formatUsd(12345.6)).toBe("$12,346");
  });
});

describe("deltas and ratios", () => {
  it("returns null against an empty previous period", () => {
    expect(deltaPct(5, 0)).toBeNull();
    expect(deltaPct(15, 10)).toBeCloseTo(50);
  });
  it("cache hit ratio over input-side tokens", () => {
    expect(cacheHitRatio({ input: 10, cache_read: 80, cache_write: 10 })).toBeCloseTo(0.8);
    expect(cacheHitRatio({ input: 0, cache_read: 0, cache_write: 0 })).toBeNull();
  });
});

describe("pivotSeries", () => {
  const pts = [
    { bucket: "2026-09-01", vendor: "claude", tokens: 100, cache_read: 0, cost_usd: 1 },
    { bucket: "2026-09-01", vendor: "codex", tokens: 50, cache_read: 0, cost_usd: 0.5 },
    { bucket: "2026-09-03", vendor: "claude", tokens: 10, cache_read: 0, cost_usd: 0.1 },
  ];
  it("fills missing days with zeros", () => {
    const out = pivotSeries(pts, "cost", "2026-09-01", "2026-09-03");
    expect(out.map((r) => r.bucket)).toEqual(["2026-09-01", "2026-09-02", "2026-09-03"]);
    expect(out[0]).toEqual({ bucket: "2026-09-01", claude: 1, codex: 0.5 });
    expect(out[1].claude + out[1].codex).toBe(0);
  });
  it("steps by week", () => {
    const out = pivotSeries([], "tokens", "2026-08-31", "2026-09-14", 7);
    expect(out.map((r) => r.bucket)).toEqual(["2026-08-31", "2026-09-07", "2026-09-14"]);
  });
  it("finds the Monday", () => {
    expect(weekStart("2026-09-03")).toBe("2026-08-31");
    expect(weekStart("2026-08-31")).toBe("2026-08-31");
    expect(weekStart("2026-09-06")).toBe("2026-08-31");
  });
});

describe("heatmap", () => {
  it("builds a Monday-first grid and five steps", () => {
    const g = heatGrid([{ dow: 0, hour: 9, tokens: 10, cost_usd: 2 }], "cost");
    expect(g[0][9]).toBe(2);
    expect(heatStep(0, 10)).toBe(0);
    expect(heatStep(1, 10)).toBe(1);
    expect(heatStep(10, 10)).toBe(5);
  });
});

describe("formatTick", () => {
  it("keeps axis labels short", () => {
    expect(formatTick("cost", 500)).toBe("$500");
    expect(formatTick("cost", 2500)).toBe("$2.5k");
    expect(formatTick("cost", 6000)).toBe("$6k");
    expect(formatTick("cost", 0.25)).toBe("$0.25");
    expect(formatTick("tokens", 2_000_000)).toBe("2.0M");
  });
});

describe("niceTicks", () => {
  it("covers the max with clean steps", () => {
    expect(niceTicks(87)).toEqual([0, 25, 50, 75, 100]);
    expect(niceTicks(0)).toEqual([0]);
  });
});

describe("price provenance", () => {
  it("marks dollars priced from a third-party estimate", () => {
    expect(thirdPartyNote({ third_party_usd: 0 })).toBeNull();
    expect(thirdPartyNote({ third_party_usd: 1.2 })).toBe("incl. $1.20 at a third-party estimate");
  });
  it("names the source URL and retrieval date of each estimate", () => {
    const base = {
      matched_key: "m",
      input: 1,
      output: 1,
      cache_read: 1,
      cache_write_5m: 1,
      cache_write_1h: 1,
      cache_write_inferred: true,
      cache_read_inferred: true,
      long_context_above: null,
      lc_input: null,
      lc_output: null,
      lc_cache_read: null,
      lc_cache_write: null,
    };
    const tip = thirdPartySources([
      { ...base, model: "review-model", basis: "third-party-estimate", source_url: "https://example.com/p", retrieved: "2026-09-24" },
      { ...base, model: "listed", basis: "list-price", source_url: "https://example.com/l", retrieved: "2026-09-24" },
    ]);
    expect(tip).toContain("review-model");
    expect(tip).toContain("https://example.com/p");
    expect(tip).toContain("2026-09-24");
    expect(tip).not.toContain("listed");
  });
  it("keeps list prices and estimates apart in wording", () => {
    expect(basisLabel("list-price")).toBe("API list price");
    expect(basisLabel("third-party-estimate")).toBe("third-party estimate");
  });
});

describe("range bounds and retention", () => {
  it("always warns for all time when Claude is in scope", () => {
    // The first bucket being after completeSince must not hide it.
    expect(claudeRetentionWarning({ vendor: "all", days: 0, from: ["2026-09-20"], completeSince: "2026-08-25" })).toBe(true);
    expect(claudeRetentionWarning({ vendor: "claude", days: 0, from: [], completeSince: "2026-08-25" })).toBe(true);
    expect(claudeRetentionWarning({ vendor: "codex", days: 0, from: [], completeSince: "2026-08-25" })).toBe(false);
  });
  it("warns for a bounded range only when it reaches before completeness", () => {
    expect(claudeRetentionWarning({ vendor: "all", days: 7, from: ["2026-09-18"], completeSince: "2026-08-25" })).toBe(false);
    expect(claudeRetentionWarning({ vendor: "all", days: 90, from: ["2026-06-27"], completeSince: "2026-08-25" })).toBe(true);
  });
  it("lists the server's day bounds without the browser timezone", () => {
    expect(dayRange("2026-09-22", "2026-09-24")).toEqual(["2026-09-22", "2026-09-23", "2026-09-24"]);
    expect(dayRange("2026-09-25", "2026-09-24")).toEqual([]);
  });
});

describe("partial refresh", () => {
  it("states what a finished refresh left out", () => {
    expect(partialRefreshNote(null)).toBeNull();
    expect(partialRefreshNote({ files_failed: 0, malformed_lines: 0, oversized_lines: 0 })).toBeNull();
    expect(partialRefreshNote({ files_failed: 2, malformed_lines: 1, oversized_lines: 0 })).toBe(
      "2 files not read, 1 malformed usage line",
    );
  });
});
