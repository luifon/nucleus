// A small Markdown parser for agent output on the dashboard (the intake
// plan and thread, ADR-036). It covers what the agents write: ATX
// headings, paragraphs, bullet / numbered / task lists with nesting,
// fenced code, block quotes, tables, rules, and inline code, bold,
// italic, strikethrough and links. It builds a tree that
// `components/Markdown.tsx` renders as React elements; no raw HTML is
// ever passed through (HTML in the source is shown as text).

export type Inline =
  | { t: "text"; v: string }
  | { t: "code"; v: string }
  | { t: "strong"; c: Inline[] }
  | { t: "em"; c: Inline[] }
  | { t: "del"; c: Inline[] }
  | { t: "link"; href: string; c: Inline[] }
  | { t: "br" };

export type ListItem = { checked: boolean | null; blocks: Block[] };

export type Block =
  | { t: "heading"; level: number; c: Inline[] }
  | { t: "para"; c: Inline[] }
  | { t: "code"; lang: string; v: string }
  | { t: "quote"; blocks: Block[] }
  | { t: "list"; ordered: boolean; start: number; items: ListItem[] }
  | { t: "table"; align: ("left" | "center" | "right" | null)[]; head: Inline[][]; rows: Inline[][][] }
  | { t: "hr" };

const FENCE = /^ {0,3}(`{3,}|~{3,})\s*([^`\s]*)[^`]*$/;
const HEADING = /^ {0,3}(#{1,6})(?:\s+(.*?))?\s*#*\s*$/;
const HR = /^ {0,3}([-*_])(?:\s*\1){2,}\s*$/;
const QUOTE = /^ {0,3}> ?(.*)$/;
const BULLET = /^( *)([-*+])\s+(.*)$/;
const ORDERED = /^( *)(\d{1,9})[.)]\s+(.*)$/;
const TABLE_SEP = /^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$/;

function isBlank(line: string): boolean {
  return line.trim() === "";
}

function listMarker(line: string): { indent: number; ordered: boolean; start: number; rest: string } | null {
  const b = BULLET.exec(line);
  if (b && !HR.test(line)) return { indent: b[1].length, ordered: false, start: 1, rest: b[3] };
  const o = ORDERED.exec(line);
  if (o) return { indent: o[1].length, ordered: true, start: Number(o[2]), rest: o[3] };
  return null;
}

function splitRow(line: string): string[] {
  let s = line.trim();
  if (s.startsWith("|")) s = s.slice(1);
  if (s.endsWith("|") && !s.endsWith("\\|")) s = s.slice(0, -1);
  const cells: string[] = [];
  let cur = "";
  for (let i = 0; i < s.length; i++) {
    if (s[i] === "\\" && s[i + 1] === "|") {
      cur += "|";
      i++;
    } else if (s[i] === "|") {
      cells.push(cur.trim());
      cur = "";
    } else {
      cur += s[i];
    }
  }
  cells.push(cur.trim());
  return cells;
}

/** Whether a line starts a block other than a paragraph (so a paragraph
 *  ends before it). */
function startsBlock(line: string): boolean {
  return FENCE.test(line) || HEADING.test(line) || HR.test(line) || QUOTE.test(line) || listMarker(line) !== null;
}

export function parseMarkdown(src: string): Block[] {
  return parseBlocks(src.replace(/\r\n?/g, "\n").replace(/\t/g, "    ").split("\n"));
}

function parseBlocks(lines: string[]): Block[] {
  const out: Block[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (isBlank(line)) {
      i++;
      continue;
    }

    const fence = FENCE.exec(line);
    if (fence) {
      const marker = fence[1];
      const body: string[] = [];
      i++;
      while (i < lines.length && !new RegExp(`^ {0,3}${marker[0]}{${marker.length},}\\s*$`).test(lines[i])) {
        body.push(lines[i++]);
      }
      i++; // closing fence (or end of input)
      out.push({ t: "code", lang: fence[2] ?? "", v: body.join("\n") });
      continue;
    }

    const heading = HEADING.exec(line);
    if (heading) {
      out.push({ t: "heading", level: heading[1].length, c: parseInline(heading[2] ?? "") });
      i++;
      continue;
    }

    if (HR.test(line)) {
      out.push({ t: "hr" });
      i++;
      continue;
    }

    if (QUOTE.test(line)) {
      const body: string[] = [];
      while (i < lines.length && !isBlank(lines[i])) {
        const q = QUOTE.exec(lines[i]);
        if (!q && startsBlock(lines[i])) break;
        body.push(q ? q[1] : lines[i]);
        i++;
      }
      out.push({ t: "quote", blocks: parseBlocks(body) });
      continue;
    }

    const marker = listMarker(line);
    if (marker) {
      const [list, next] = parseList(lines, i, marker.indent, marker.ordered, marker.start);
      out.push(list);
      i = next;
      continue;
    }

    if (line.includes("|") && i + 1 < lines.length && TABLE_SEP.test(lines[i + 1]) && lines[i + 1].includes("-")) {
      const head = splitRow(line);
      const align = splitRow(lines[i + 1]).map((c) => {
        const l = c.startsWith(":");
        const r = c.endsWith(":");
        return l && r ? "center" : r ? "right" : l ? "left" : null;
      });
      i += 2;
      const rows: Inline[][][] = [];
      while (i < lines.length && !isBlank(lines[i]) && lines[i].includes("|")) {
        const cells = splitRow(lines[i++]);
        rows.push(head.map((_, k) => parseInline(cells[k] ?? "")));
      }
      out.push({ t: "table", align: head.map((_, k) => align[k] ?? null), head: head.map(parseInline), rows });
      continue;
    }

    const para: string[] = [];
    while (i < lines.length && !isBlank(lines[i]) && (para.length === 0 || !startsBlock(lines[i]))) {
      para.push(lines[i++]);
    }
    out.push({ t: "para", c: parseParagraph(para) });
  }
  return out;
}

/** A list starting at `lines[i]`: items at `indent`; deeper-indented lines
 *  belong to the current item (nested lists, continuation paragraphs). */
function parseList(lines: string[], i: number, indent: number, ordered: boolean, start: number): [Block, number] {
  const items: ListItem[] = [];
  while (i < lines.length) {
    const m = listMarker(lines[i]);
    if (!m || m.indent !== indent || m.ordered !== ordered) break;
    let rest = m.rest;
    let checked: boolean | null = null;
    const task = /^\[([ xX])\]\s+(.*)$/.exec(rest);
    if (task) {
      checked = task[1] !== " ";
      rest = task[2];
    }
    const body: string[] = [rest];
    i++;
    // Content column: where the item text starts; nested lines are
    // re-indented relative to it.
    const inner = indent + 2;
    while (i < lines.length) {
      const l = lines[i];
      if (isBlank(l)) {
        // A blank line continues the item only when the next non-blank
        // line is indented into it.
        let k = i + 1;
        while (k < lines.length && isBlank(lines[k])) k++;
        if (k < lines.length && leadingSpaces(lines[k]) >= inner) {
          body.push("");
          i++;
          continue;
        }
        break;
      }
      const lm = listMarker(l);
      if (lm && lm.indent <= indent) break;
      if (!lm && leadingSpaces(l) < inner && startsBlock(l)) break;
      body.push(leadingSpaces(l) >= inner ? l.slice(inner) : l.trimStart());
      i++;
    }
    items.push({ checked, blocks: parseBlocks(body) });
  }
  return [{ t: "list", ordered, start, items }, i];
}

function leadingSpaces(line: string): number {
  return line.length - line.trimStart().length;
}

/** Paragraph lines joined; a line ending in two spaces or a backslash is a
 *  hard break, others join with a space. */
function parseParagraph(lines: string[]): Inline[] {
  const out: Inline[] = [];
  lines.forEach((raw, k) => {
    const hard = / {2,}$/.test(raw) || /\\$/.test(raw);
    const line = raw.trim().replace(/\\$/, "");
    out.push(...parseInline(line));
    if (k < lines.length - 1) out.push(hard ? { t: "br" } : { t: "text", v: " " });
  });
  return merge(out);
}

function merge(nodes: Inline[]): Inline[] {
  const out: Inline[] = [];
  for (const n of nodes) {
    const last = out[out.length - 1];
    if (n.t === "text" && last?.t === "text") last.v += n.v;
    else out.push(n);
  }
  return out;
}

/** Only http(s) and mailto links are rendered as links. */
export function safeHref(href: string): string | null {
  const h = href.trim();
  return /^(https?:\/\/|mailto:)/i.test(h) ? h : null;
}

const ESCAPABLE = "\\`*_{}[]()#+-.!|~>";

export function parseInline(src: string): Inline[] {
  const out: Inline[] = [];
  let text = "";
  const flush = () => {
    if (text) out.push({ t: "text", v: text });
    text = "";
  };
  let i = 0;
  while (i < src.length) {
    const ch = src[i];

    if (ch === "\\" && i + 1 < src.length && ESCAPABLE.includes(src[i + 1])) {
      text += src[i + 1];
      i += 2;
      continue;
    }

    if (ch === "`") {
      let n = 1;
      while (src[i + n] === "`") n++;
      const ticks = "`".repeat(n);
      const end = src.indexOf(ticks, i + n);
      if (end !== -1) {
        flush();
        let v = src.slice(i + n, end);
        if (v.startsWith(" ") && v.endsWith(" ") && v.trim()) v = v.slice(1, -1);
        out.push({ t: "code", v });
        i = end + n;
        continue;
      }
      text += ticks;
      i += n;
      continue;
    }

    if (ch === "[") {
      const close = matchBracket(src, i);
      if (close !== -1 && src[close + 1] === "(") {
        const end = src.indexOf(")", close + 2);
        if (end !== -1) {
          const href = safeHref(src.slice(close + 2, end).split(/\s+/)[0] ?? "");
          const label = parseInline(src.slice(i + 1, close));
          flush();
          if (href) out.push({ t: "link", href, c: label });
          else out.push(...label);
          i = end + 1;
          continue;
        }
      }
    }

    if ((ch === "h" || ch === "H") && /^https?:\/\//i.test(src.slice(i, i + 8)) && !/[\w/]/.test(src[i - 1] ?? "")) {
      const m = /^https?:\/\/[^\s<>()]+/i.exec(src.slice(i));
      if (m) {
        const url = m[0].replace(/[.,;:!?'"]+$/, "");
        flush();
        out.push({ t: "link", href: url, c: [{ t: "text", v: url }] });
        i += url.length;
        continue;
      }
    }

    if (ch === "~" && src[i + 1] === "~") {
      const end = src.indexOf("~~", i + 2);
      if (end > i + 2) {
        flush();
        out.push({ t: "del", c: parseInline(src.slice(i + 2, end)) });
        i = end + 2;
        continue;
      }
    }

    if (ch === "*" || ch === "_") {
      const double = src[i + 1] === ch;
      const delim = double ? ch + ch : ch;
      const prev = src[i - 1] ?? " ";
      const next = src[i + delim.length] ?? " ";
      // `_` inside a word (snake_case) is text; a delimiter must be
      // followed by a non-space.
      const opens = !/\s/.test(next) && !(ch === "_" && /\w/.test(prev));
      if (opens) {
        const end = findClose(src, i + delim.length, delim);
        if (end !== -1) {
          flush();
          const c = parseInline(src.slice(i + delim.length, end));
          out.push(double ? { t: "strong", c } : { t: "em", c });
          i = end + delim.length;
          continue;
        }
      }
    }

    text += ch;
    i++;
  }
  flush();
  return out;
}

function matchBracket(src: string, open: number): number {
  let depth = 0;
  for (let k = open; k < src.length; k++) {
    if (src[k] === "\\") {
      k++;
      continue;
    }
    if (src[k] === "`") {
      const end = src.indexOf("`", k + 1);
      if (end !== -1) k = end;
      continue;
    }
    if (src[k] === "[") depth++;
    else if (src[k] === "]" && --depth === 0) return k;
  }
  return -1;
}

/** The closing delimiter: preceded by a non-space, and for `_` not followed
 *  by a word character. Code spans are skipped. */
function findClose(src: string, from: number, delim: string): number {
  for (let k = from; k < src.length; k++) {
    if (src[k] === "\\") {
      k++;
      continue;
    }
    if (src[k] === "`") {
      const end = src.indexOf("`", k + 1);
      if (end !== -1) k = end;
      continue;
    }
    // A single delimiter skips a doubled one (`*a **b** c*`).
    if (delim.length === 1 && src.startsWith(delim + delim, k)) {
      k++;
      continue;
    }
    if (src.startsWith(delim, k) && k > from && !/\s/.test(src[k - 1])) {
      if (delim[0] === "_" && /\w/.test(src[k + delim.length] ?? "")) continue;
      return k;
    }
  }
  return -1;
}
