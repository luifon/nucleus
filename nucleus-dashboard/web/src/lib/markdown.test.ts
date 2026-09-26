import { describe, expect, test } from "vitest";
import { parseInline, parseMarkdown, safeHref } from "./markdown";

describe("blocks", () => {
  test("headings, paragraphs, rules and code fences", () => {
    const doc = parseMarkdown("# Plan\n\nFirst line\nsame paragraph\n\n---\n\n```rust\nfn main() {}\n\n// x\n```\nafter");
    expect(doc.map((b) => b.t)).toEqual(["heading", "para", "hr", "code", "para"]);
    expect(doc[0]).toEqual({ t: "heading", level: 1, c: [{ t: "text", v: "Plan" }] });
    expect(doc[1]).toEqual({ t: "para", c: [{ t: "text", v: "First line same paragraph" }] });
    expect(doc[3]).toEqual({ t: "code", lang: "rust", v: "fn main() {}\n\n// x" });
  });

  test("nested and task lists", () => {
    const doc = parseMarkdown("1. one\n2. two\n   - a\n   - [x] b\n3. three\n\ntext");
    expect(doc.map((b) => b.t)).toEqual(["list", "para"]);
    const list = doc[0];
    if (list.t !== "list") throw new Error("not a list");
    expect(list.ordered).toBe(true);
    expect(list.items).toHaveLength(3);
    const nested = list.items[1].blocks[1];
    if (nested.t !== "list") throw new Error("not nested");
    expect(nested.items.map((i) => i.checked)).toEqual([null, true]);
  });

  test("tables with alignment and escaped pipes", () => {
    const doc = parseMarkdown("| file | change |\n|:--|--:|\n| a.rs | x \\| y |\n| b.rs |");
    const t = doc[0];
    if (t.t !== "table") throw new Error("not a table");
    expect(t.align).toEqual(["left", "right"]);
    expect(t.rows).toHaveLength(2);
    expect(t.rows[0][1]).toEqual([{ t: "text", v: "x | y" }]);
    expect(t.rows[1][1]).toEqual([]);
  });

  test("block quotes and raw HTML stay text", () => {
    const doc = parseMarkdown("> quoted\n> more\n\n<script>x</script>");
    expect(doc[0].t).toBe("quote");
    expect(doc[1]).toEqual({ t: "para", c: [{ t: "text", v: "<script>x</script>" }] });
  });
});

describe("inline", () => {
  test("emphasis, code and strikethrough", () => {
    expect(parseInline("**b** *i* `c*d` ~~s~~")).toEqual([
      { t: "strong", c: [{ t: "text", v: "b" }] },
      { t: "text", v: " " },
      { t: "em", c: [{ t: "text", v: "i" }] },
      { t: "text", v: " " },
      { t: "code", v: "c*d" },
      { t: "text", v: " " },
      { t: "del", c: [{ t: "text", v: "s" }] },
    ]);
    expect(parseInline("snake_case_name")).toEqual([{ t: "text", v: "snake_case_name" }]);
    expect(parseInline("*a **b** c*")[0].t).toBe("em");
  });

  test("links: only http(s) and mailto are links", () => {
    expect(parseInline("[pr](https://example.invalid/pull/1)")).toEqual([
      { t: "link", href: "https://example.invalid/pull/1", c: [{ t: "text", v: "pr" }] },
    ]);
    expect(parseInline("[x](javascript:alert(1))")[0]).toEqual({ t: "text", v: "x" });
    expect(parseInline("see https://example.invalid/a.")).toEqual([
      { t: "text", v: "see " },
      { t: "link", href: "https://example.invalid/a", c: [{ t: "text", v: "https://example.invalid/a" }] },
      { t: "text", v: "." },
    ]);
    expect(safeHref(" mailto:someone@example.invalid")).toBe("mailto:someone@example.invalid");
  });
});
