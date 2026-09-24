// Pure helpers for the Vault page (ADR-035): snippet highlighting, finding
// grouping, and run-to-run deltas for the check trend.

import type { CheckCounts, CheckRunSummary, VaultFinding } from "@/lib/api/vault";

/** Match markers in search snippets (core `vault::index::MATCH_START/END`). */
export const MATCH_START = "\u0002";
export const MATCH_END = "\u0003";

export type SnippetPart = { text: string; match: boolean };

/** Split a snippet into plain and matched parts. An unterminated marker
 *  treats the rest of the text as matched. */
export function splitSnippet(snippet: string): SnippetPart[] {
  const parts: SnippetPart[] = [];
  let rest = snippet;
  while (rest.length > 0) {
    const start = rest.indexOf(MATCH_START);
    if (start < 0) {
      parts.push({ text: rest, match: false });
      break;
    }
    if (start > 0) parts.push({ text: rest.slice(0, start), match: false });
    const after = rest.slice(start + 1);
    const end = after.indexOf(MATCH_END);
    if (end < 0) {
      if (after) parts.push({ text: after, match: true });
      break;
    }
    if (end > 0) parts.push({ text: after.slice(0, end), match: true });
    rest = after.slice(end + 1);
  }
  return parts;
}

/** Finding kinds in display order, with the operator-facing label. */
export const FINDING_KINDS: { kind: string; label: string }[] = [
  { kind: "duplicate_name", label: "same file name" },
  { kind: "similar_title", label: "similar titles" },
  { kind: "dated_series", label: "dated series" },
  { kind: "duplicate_content", label: "same content" },
  { kind: "broken_link", label: "broken links" },
  { kind: "orphan", label: "orphans" },
  { kind: "stale_inbox", label: "stale inbox" },
  { kind: "frontmatter", label: "frontmatter" },
  { kind: "unknown_source", label: "unknown source" },
  { kind: "empty_file", label: "empty files" },
  { kind: "oversized", label: "oversized, not checked" },
];

export type FindingGroup = { kind: string; label: string; findings: VaultFinding[] };

/** Group findings by kind in display order; unknown kinds go last. */
export function groupFindings(findings: VaultFinding[]): FindingGroup[] {
  const known = new Set(FINDING_KINDS.map((k) => k.kind));
  const groups: FindingGroup[] = FINDING_KINDS.map((k) => ({
    ...k,
    findings: findings.filter((f) => f.kind === k.kind),
  }));
  const other = findings.filter((f) => !known.has(f.kind));
  if (other.length) groups.push({ kind: "other", label: "other", findings: other });
  return groups.filter((g) => g.findings.length > 0);
}

/** The count columns shown as tiles and trend columns. */
export const COUNT_COLUMNS: { key: keyof CheckCounts; label: string }[] = [
  { key: "duplicates", label: "duplicates" },
  { key: "broken_links", label: "broken links" },
  { key: "orphans", label: "orphans" },
  { key: "stale_inbox", label: "stale inbox" },
  { key: "missing_frontmatter", label: "frontmatter" },
  { key: "unknown_source", label: "unknown source" },
  { key: "empty_files", label: "empty files" },
  { key: "oversized", label: "oversized" },
  { key: "fixed", label: "fixed" },
];

/** Change of one count from the previous run (runs are newest first).
 *  `null` when there is no earlier run. */
export function countDelta(runs: CheckRunSummary[], key: keyof CheckCounts): number | null {
  if (runs.length < 2) return null;
  return runs[0].counts[key] - runs[1].counts[key];
}

/** `+3` / `−2` / `±0`. */
export function formatDelta(d: number): string {
  if (d > 0) return `+${d}`;
  if (d < 0) return `−${Math.abs(d)}`;
  return "±0";
}
