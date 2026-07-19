// Canvas (ADR-012): agent-emitted interactive blocks in chat messages.
//
// Grammar (inline in assistant text, one block per interaction):
//
//   <canvas v="1" type="decision" id="pick-bucket" title="Which bucket?">
//   { ...JSON payload, shape per type... }
//   </canvas>
//
// `v` and `id` are mandatory (ADR-012 field notes: version from day one,
// ids from day one so update-ops can arrive later without a format break).
// A block that fails ANY parse step degrades to a fallback segment carrying
// the raw text — malformed agent output must never break the transcript.
//
// The user's interaction posts back through the normal message POST as:
//
//   <canvas-response v="1" id="pick-bucket" type="decision">
//   {"choice":"slipbox"}
//   </canvas-response>
//
// Answered-state is derived purely from the message list (a later user
// message containing a canvas-response for the block's id) — no separate
// store, so re-rendering from history is identical to live rendering by
// construction.

export const CANVAS_VERSION = 1;

export type CanvasType = "decision" | "multi-select" | "confirm" | "form" | "review";

export interface CanvasOption {
  key: string;
  label: string;
  hint?: string;
  checked?: boolean;
  detail?: string;
}

export interface CanvasField {
  key: string;
  label: string;
  kind?: "text" | "date" | "time" | "number";
  placeholder?: string;
  value?: string;
}

export interface CanvasBlockData {
  v: number;
  type: CanvasType;
  id: string;
  title?: string;
  /** decision / multi-select / review */
  options?: CanvasOption[];
  /** confirm */
  prompt?: string;
  danger?: boolean;
  /** form */
  fields?: CanvasField[];
}

/** A parsed message: interleaved text and canvas segments, in order. */
export type MessageSegment =
  | { kind: "text"; text: string }
  | { kind: "canvas"; block: CanvasBlockData; raw: string }
  | { kind: "canvas-fallback"; reason: string; raw: string };

const BLOCK_RE = /<canvas\s+([^>]*)>([\s\S]*?)<\/canvas>/g;
const RESPONSE_RE = /<canvas-response\s+([^>]*)>([\s\S]*?)<\/canvas-response>/g;
const ATTR_RE = /([a-zA-Z-]+)\s*=\s*"([^"]*)"/g;

function parseAttrs(s: string): Record<string, string> {
  const out: Record<string, string> = {};
  for (const m of s.matchAll(ATTR_RE)) out[m[1]] = m[2];
  return out;
}

const KNOWN_TYPES: CanvasType[] = ["decision", "multi-select", "confirm", "form", "review"];

function parseBlock(attrText: string, body: string, raw: string): MessageSegment {
  const attrs = parseAttrs(attrText);
  const v = Number(attrs["v"]);
  if (!Number.isInteger(v) || v < 1) {
    return { kind: "canvas-fallback", reason: "missing or invalid v attribute", raw };
  }
  if (v > CANVAS_VERSION) {
    return { kind: "canvas-fallback", reason: `unsupported canvas block (v${v})`, raw };
  }
  const id = attrs["id"]?.trim();
  if (!id) {
    return { kind: "canvas-fallback", reason: "missing id attribute", raw };
  }
  const type = attrs["type"] as CanvasType;
  if (!KNOWN_TYPES.includes(type)) {
    return { kind: "canvas-fallback", reason: `unknown canvas type (${attrs["type"] ?? "?"})`, raw };
  }
  let payload: unknown;
  try {
    payload = JSON.parse(body.trim() === "" ? "{}" : body);
  } catch {
    return { kind: "canvas-fallback", reason: "payload is not valid JSON", raw };
  }
  if (typeof payload !== "object" || payload === null || Array.isArray(payload)) {
    return { kind: "canvas-fallback", reason: "payload must be a JSON object", raw };
  }
  const p = payload as Record<string, unknown>;
  const block: CanvasBlockData = {
    v,
    type,
    id,
    title: attrs["title"],
    options: p["options"] as CanvasOption[] | undefined,
    prompt: p["prompt"] as string | undefined,
    danger: p["danger"] as boolean | undefined,
    fields: p["fields"] as CanvasField[] | undefined,
  };
  // Per-type minimal shape guard — a block we can't render meaningfully
  // falls back rather than rendering an empty widget.
  const optionish = block.type === "decision" || block.type === "multi-select" || block.type === "review";
  if (optionish && (!Array.isArray(block.options) || block.options.length === 0)) {
    return { kind: "canvas-fallback", reason: `${type} block has no options`, raw };
  }
  if (block.type === "form" && (!Array.isArray(block.fields) || block.fields.length === 0)) {
    return { kind: "canvas-fallback", reason: "form block has no fields", raw };
  }
  return { kind: "canvas", block, raw };
}

/** Split an assistant message into ordered text + canvas segments. */
export function parseMessage(content: string): MessageSegment[] {
  const segments: MessageSegment[] = [];
  let last = 0;
  BLOCK_RE.lastIndex = 0;
  for (const m of content.matchAll(BLOCK_RE)) {
    const idx = m.index ?? 0;
    if (idx > last) {
      const text = content.slice(last, idx);
      if (text.trim() !== "") segments.push({ kind: "text", text });
    }
    segments.push(parseBlock(m[1], m[2], m[0]));
    last = idx + m[0].length;
  }
  if (last < content.length) {
    const text = content.slice(last);
    if (text.trim() !== "") segments.push({ kind: "text", text });
  }
  return segments;
}

// ── responses ───────────────────────────────────────────────────────────

export type CanvasResponseValue =
  | { choice: string }
  | { selected: string[]; unselected: string[] }
  | { confirmed: boolean }
  | { values: Record<string, string> };

/** Build the message text a canvas interaction posts back. */
export function buildResponse(block: CanvasBlockData, value: CanvasResponseValue): string {
  return (
    `<canvas-response v="${CANVAS_VERSION}" id="${block.id}" type="${block.type}">\n` +
    `${JSON.stringify(value)}\n` +
    `</canvas-response>`
  );
}

export interface ParsedResponse {
  id: string;
  type: string;
  value: unknown;
}

/** Parse canvas-responses out of a (user) message, if any. */
export function parseResponses(content: string): ParsedResponse[] {
  const out: ParsedResponse[] = [];
  RESPONSE_RE.lastIndex = 0;
  for (const m of content.matchAll(RESPONSE_RE)) {
    const attrs = parseAttrs(m[1]);
    if (!attrs["id"]) continue;
    let value: unknown = null;
    try {
      value = JSON.parse(m[2]);
    } catch {
      // keep null — the id is still what marks the block answered
    }
    out.push({ id: attrs["id"], type: attrs["type"] ?? "", value });
  }
  return out;
}

/** Human-readable one-liner for a submitted response (user-bubble chip). */
export function describeResponse(r: ParsedResponse): string {
  const v = r.value as Record<string, unknown> | null;
  if (v && typeof v === "object") {
    if (typeof v["choice"] === "string") return `${r.id}: ${v["choice"]}`;
    if (Array.isArray(v["selected"])) return `${r.id}: ${(v["selected"] as string[]).join(", ") || "(none)"}`;
    if (typeof v["confirmed"] === "boolean") return `${r.id}: ${v["confirmed"] ? "confirmed" : "declined"}`;
    if (v["values"] && typeof v["values"] === "object")
      return `${r.id}: ${Object.entries(v["values"] as Record<string, string>)
        .map(([k, val]) => `${k}=${val}`)
        .join(", ")}`;
  }
  return r.id;
}

/**
 * The ids of every canvas block already answered somewhere in `messages`
 * (role/user content pairs). Derived state — this is the ONLY source of
 * answered-ness, so history re-render and live render agree by
 * construction (ADR-012 field note 3).
 */
export function answeredIds(messages: { role: string; content: string }[]): Set<string> {
  const ids = new Set<string>();
  for (const m of messages) {
    if (m.role !== "user") continue;
    if (!m.content.includes("<canvas-response")) continue;
    for (const r of parseResponses(m.content)) ids.add(r.id);
  }
  return ids;
}
