import { describe, expect, test } from "vitest";
import { collapseUnchanged, diffStats, lineDiff, splitLines } from "./linediff";

const kinds = (before: string, after: string) => lineDiff(before, after).map((o) => `${o.kind}:${o.text}`);

describe("lineDiff", () => {
  test("unchanged text is all same lines", () => {
    expect(kinds("a\nb\n", "a\nb\n")).toEqual(["same:a", "same:b"]);
  });
  test("added lines", () => {
    expect(kinds("a\nc", "a\nb\nc\nd")).toEqual(["same:a", "add:b", "same:c", "add:d"]);
  });
  test("removed lines", () => {
    expect(kinds("a\nb\nc", "a\nc")).toEqual(["same:a", "del:b", "same:c"]);
  });
  test("a changed line is a removal followed by an addition", () => {
    expect(kinds("a\nold\nz", "a\nnew\nz")).toEqual(["same:a", "del:old", "add:new", "same:z"]);
  });
  test("empty input on either side", () => {
    expect(lineDiff("", "")).toEqual([]);
    expect(kinds("", "x\ny")).toEqual(["add:x", "add:y"]);
    expect(kinds("x\ny", "")).toEqual(["del:x", "del:y"]);
  });
  test("line endings do not count as changes", () => {
    expect(kinds("a\r\nb\r\n", "a\nb")).toEqual(["same:a", "same:b"]);
    expect(splitLines("a\n\nb\n")).toEqual(["a", "", "b"]);
  });
  test("keeps the longest common run", () => {
    const ops = lineDiff("1\n2\n3\n4\n5", "0\n2\n3\n4\n6");
    expect(ops.filter((o) => o.kind === "same").map((o) => o.text)).toEqual(["2", "3", "4"]);
    expect(diffStats(ops)).toEqual({ added: 2, removed: 2 });
  });
});

describe("collapseUnchanged", () => {
  test("long unchanged runs become one skip row, with context kept around changes", () => {
    const before = Array.from({ length: 20 }, (_, i) => `l${i}`).join("\n");
    const after = before.replace("l10", "changed");
    const rows = collapseUnchanged(lineDiff(before, after), 2);
    expect(rows[0]).toEqual({ kind: "skip", count: 8 });
    expect(rows.slice(1, 7).map((r) => ("text" in r ? r.text : ""))).toEqual(["l8", "l9", "l10", "changed", "l11", "l12"]);
    expect(rows[7]).toEqual({ kind: "skip", count: 7 });
  });
  test("no changes collapses to one skip row", () => {
    expect(collapseUnchanged(lineDiff("a\nb", "a\nb"))).toEqual([{ kind: "skip", count: 2 }]);
  });
});
