import { describe, expect, test } from "vitest";
import type { CheckCounts, CheckRunSummary, VaultFinding } from "@/lib/api/vault";
import { countDelta, formatDelta, groupFindings, MATCH_END, MATCH_START, splitSnippet } from "./vault";

const S = MATCH_START;
const E = MATCH_END;

describe("splitSnippet", () => {
  test("plain text", () => {
    expect(splitSnippet("no match here")).toEqual([{ text: "no match here", match: false }]);
  });
  test("matches between markers, brackets of links untouched", () => {
    expect(splitSnippet(`see [[note]] about ${S}rocket${E} engines`)).toEqual([
      { text: "see [[note]] about ", match: false },
      { text: "rocket", match: true },
      { text: " engines", match: false },
    ]);
  });
  test("adjacent and unterminated markers", () => {
    expect(splitSnippet(`${S}a${E}${S}b${E}`)).toEqual([
      { text: "a", match: true },
      { text: "b", match: true },
    ]);
    expect(splitSnippet(`x ${S}tail`)).toEqual([
      { text: "x ", match: false },
      { text: "tail", match: true },
    ]);
  });
});

function finding(kind: string): VaultFinding {
  return { kind, path: "a.md", detail: "", related: [], fixed: false, fix_action: null };
}

test("groupFindings orders by kind and drops empty groups", () => {
  const groups = groupFindings([finding("orphan"), finding("broken_link"), finding("orphan"), finding("new_kind")]);
  expect(groups.map((g) => [g.kind, g.findings.length])).toEqual([
    ["broken_link", 1],
    ["orphan", 2],
    ["other", 1],
  ]);
});

function run(id: number, broken: number): CheckRunSummary {
  const counts: CheckCounts = {
    duplicates: 0,
    broken_links: broken,
    orphans: 0,
    stale_inbox: 0,
    missing_frontmatter: 0,
    unknown_source: 0,
    empty_files: 0,
    fixed: 0,
  };
  return { id, started_at: "2026-01-01T00:00:00.000Z", trigger: "scheduled", applied: false, notes_scanned: 1, duration_ms: 1, counts };
}

test("countDelta compares the newest run with the one before", () => {
  expect(countDelta([run(2, 5)], "broken_links")).toBeNull();
  expect(countDelta([run(3, 5), run(2, 8)], "broken_links")).toBe(-3);
  expect(formatDelta(-3)).toBe("−3");
  expect(formatDelta(2)).toBe("+2");
  expect(formatDelta(0)).toBe("±0");
});
