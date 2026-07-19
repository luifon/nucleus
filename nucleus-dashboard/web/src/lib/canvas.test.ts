import { describe, expect, test } from "vitest";
import {
  answeredIds,
  buildResponse,
  describeResponse,
  parseMessage,
  parseResponses,
  type CanvasBlockData,
} from "./canvas";

const DECISION = `Before that, pick a bucket:
<canvas v="1" type="decision" id="pick-bucket" title="Which PARA bucket?">
{"options":[{"key":"inbox","label":"0-Inbox"},{"key":"slipbox","label":"6-Slipbox"}]}
</canvas>
I'll file it right after.`;

describe("parseMessage", () => {
  test("interleaves text and canvas segments in order", () => {
    const segs = parseMessage(DECISION);
    expect(segs.map((s) => s.kind)).toEqual(["text", "canvas", "text"]);
    const block = segs[1].kind === "canvas" ? segs[1].block : null;
    expect(block?.id).toBe("pick-bucket");
    expect(block?.type).toBe("decision");
    expect(block?.title).toBe("Which PARA bucket?");
    expect(block?.options?.length).toBe(2);
  });

  test("plain text yields a single text segment; empty stays empty", () => {
    expect(parseMessage("just prose")).toEqual([{ kind: "text", text: "just prose" }]);
    expect(parseMessage("")).toEqual([]);
  });

  test("multiple blocks in one message keep distinct ids", () => {
    const two = `${DECISION}\n<canvas v="1" type="confirm" id="go">\n{"prompt":"Proceed?"}\n</canvas>`;
    const segs = parseMessage(two);
    const ids = segs.filter((s) => s.kind === "canvas").map((s) => (s.kind === "canvas" ? s.block.id : ""));
    expect(ids).toEqual(["pick-bucket", "go"]);
  });

  // The never-breakage contract: every malformed shape degrades to a
  // fallback segment that carries the raw text.
  test.each([
    ["missing v", `<canvas type="decision" id="x">{"options":[{"key":"a","label":"A"}]}</canvas>`, /invalid v/],
    ["future v", `<canvas v="9" type="decision" id="x">{"options":[{"key":"a","label":"A"}]}</canvas>`, /unsupported canvas block \(v9\)/],
    ["missing id", `<canvas v="1" type="decision">{"options":[{"key":"a","label":"A"}]}</canvas>`, /missing id/],
    ["unknown type", `<canvas v="1" type="wheel" id="x">{}</canvas>`, /unknown canvas type/],
    ["bad json", `<canvas v="1" type="decision" id="x">{nope}</canvas>`, /not valid JSON/],
    ["array payload", `<canvas v="1" type="decision" id="x">[1,2]</canvas>`, /JSON object/],
    ["decision without options", `<canvas v="1" type="decision" id="x">{}</canvas>`, /no options/],
    ["form without fields", `<canvas v="1" type="form" id="x">{}</canvas>`, /no fields/],
  ])("degrades to fallback: %s", (_name, input, reason) => {
    const segs = parseMessage(input);
    expect(segs).toHaveLength(1);
    const seg = segs[0];
    expect(seg.kind).toBe("canvas-fallback");
    if (seg.kind === "canvas-fallback") {
      expect(seg.reason).toMatch(reason);
      expect(seg.raw).toBe(input);
    }
  });
});

describe("responses", () => {
  const block: CanvasBlockData = { v: 1, type: "decision", id: "pick-bucket" };

  test("build → parse round-trip", () => {
    const msg = buildResponse(block, { choice: "slipbox" });
    const parsed = parseResponses(msg);
    expect(parsed).toEqual([{ id: "pick-bucket", type: "decision", value: { choice: "slipbox" } }]);
  });

  test("describeResponse renders human-readable chips", () => {
    expect(describeResponse({ id: "b", type: "decision", value: { choice: "slipbox" } })).toBe("b: slipbox");
    expect(describeResponse({ id: "b", type: "multi-select", value: { selected: ["x", "y"], unselected: [] } })).toBe("b: x, y");
    expect(describeResponse({ id: "b", type: "multi-select", value: { selected: [], unselected: ["x"] } })).toBe("b: (none)");
    expect(describeResponse({ id: "b", type: "confirm", value: { confirmed: false } })).toBe("b: declined");
    expect(describeResponse({ id: "b", type: "form", value: { values: { date: "2026-07-19" } } })).toBe("b: date=2026-07-19");
  });
});

describe("answeredIds (history-re-render parity)", () => {
  test("answered state derives from user messages only", () => {
    const block: CanvasBlockData = { v: 1, type: "confirm", id: "go" };
    const messages = [
      { role: "assistant", content: DECISION },
      { role: "user", content: buildResponse({ v: 1, type: "decision", id: "pick-bucket" }, { choice: "slipbox" }) },
      { role: "assistant", content: `<canvas v="1" type="confirm" id="go">{"prompt":"Proceed?"}</canvas>` },
      // an assistant message MENTIONING a response must not count
      { role: "assistant", content: buildResponse(block, { confirmed: true }) },
    ];
    const ids = answeredIds(messages);
    expect(ids.has("pick-bucket")).toBe(true);
    expect(ids.has("go")).toBe(false);
  });

  test("a reloaded transcript yields the same answered set as live appends", () => {
    const live: { role: string; content: string }[] = [];
    live.push({ role: "assistant", content: DECISION });
    live.push({
      role: "user",
      content: buildResponse({ v: 1, type: "decision", id: "pick-bucket" }, { choice: "inbox" }),
    });
    const liveSet = answeredIds(live);
    // "reload": same rows deserialized fresh (structural copy)
    const reloaded = JSON.parse(JSON.stringify(live)) as typeof live;
    expect(answeredIds(reloaded)).toEqual(liveSet);
  });
});
