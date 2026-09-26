// Line diff for the intake plan panel (ADR-036): which lines of a plan
// version were added or removed relative to the version before it. A
// longest-common-subsequence table over lines; plans are at most a few
// hundred lines, so the O(n·m) table is small.

export type DiffOp = { kind: "same" | "add" | "del"; text: string };

/** Split text into lines. CRLF, CR and LF each end a line; an empty text
 *  has no lines, and a trailing line break does not add an empty line. */
export function splitLines(text: string): string[] {
  if (text === "") return [];
  const lines = text.split(/\r\n|\r|\n/);
  if (lines[lines.length - 1] === "") lines.pop();
  return lines;
}

/** The edit from `before` to `after`, line by line, in `after` order. At
 *  each change the removed lines come before the added lines. */
export function lineDiff(before: string, after: string): DiffOp[] {
  const a = splitLines(before);
  const b = splitLines(after);
  const n = a.length;
  const m = b.length;
  // lcs[i][j] = length of the LCS of a[i..] and b[j..].
  const lcs: Uint32Array[] = Array.from({ length: n + 1 }, () => new Uint32Array(m + 1));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      lcs[i][j] = a[i] === b[j] ? lcs[i + 1][j + 1] + 1 : Math.max(lcs[i + 1][j], lcs[i][j + 1]);
    }
  }
  const out: DiffOp[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      out.push({ kind: "same", text: a[i] });
      i++;
      j++;
    } else if (lcs[i + 1][j] >= lcs[i][j + 1]) {
      out.push({ kind: "del", text: a[i++] });
    } else {
      out.push({ kind: "add", text: b[j++] });
    }
  }
  while (i < n) out.push({ kind: "del", text: a[i++] });
  while (j < m) out.push({ kind: "add", text: b[j++] });
  return out;
}

export type DiffRow = DiffOp | { kind: "skip"; count: number };

/** Keep `context` unchanged lines on each side of every change and
 *  replace each longer unchanged run with one `skip` row. */
export function collapseUnchanged(ops: readonly DiffOp[], context = 3): DiffRow[] {
  const near = new Array<boolean>(ops.length).fill(false);
  ops.forEach((op, k) => {
    if (op.kind === "same") return;
    for (let d = Math.max(0, k - context); d <= Math.min(ops.length - 1, k + context); d++) near[d] = true;
  });
  const out: DiffRow[] = [];
  let skipped = 0;
  ops.forEach((op, k) => {
    if (op.kind !== "same" || near[k]) {
      if (skipped > 0) out.push({ kind: "skip", count: skipped });
      skipped = 0;
      out.push(op);
    } else {
      skipped++;
    }
  });
  if (skipped > 0) out.push({ kind: "skip", count: skipped });
  return out;
}

/** Count of added and removed lines. */
export function diffStats(ops: readonly DiffOp[]): { added: number; removed: number } {
  let added = 0;
  let removed = 0;
  for (const op of ops) {
    if (op.kind === "add") added++;
    else if (op.kind === "del") removed++;
  }
  return { added, removed };
}
