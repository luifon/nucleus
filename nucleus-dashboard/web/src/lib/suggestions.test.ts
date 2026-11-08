import { describe, expect, test } from "vitest";
import type { LiveSkill, SuggestionAcceptResp, SuggestionSummary } from "@/lib/api/skills";
import {
  acceptConfirmText,
  describeAccept,
  liveSkillNote,
  oneLine,
  refusalHead,
  splitFrontmatter,
  splitSuggestions,
  suggestionFromSearch,
  withSuggestion,
} from "./suggestions";

function summary(over: Partial<SuggestionSummary> = {}): SuggestionSummary {
  return {
    id: 1,
    kind: "create",
    name: "service-deploy",
    status: "pending",
    origin: "backfill",
    rationale: "r",
    reject_reason: null,
    superseded_by: null,
    edited: false,
    evidence_count: 1,
    created_at: "2026-11-03T15:00:00.000Z",
    updated_at: "2026-11-03T15:00:00.000Z",
    ...over,
  };
}

describe("deep link", () => {
  test("reads a positive integer id only", () => {
    expect(suggestionFromSearch(new URLSearchParams("suggestion=12"))).toBe(12);
    for (const bad of ["", "suggestion=", "suggestion=0", "suggestion=-3", "suggestion=1.5", "suggestion=abc", "suggestion=99999999999999999999"]) {
      expect(suggestionFromSearch(new URLSearchParams(bad))).toBeNull();
    }
  });

  test("sets and clears the parameter, keeping the others", () => {
    const p = new URLSearchParams("x=1");
    expect(withSuggestion(p, 4).toString()).toBe("x=1&suggestion=4");
    expect(withSuggestion(new URLSearchParams("x=1&suggestion=4"), null).toString()).toBe("x=1");
    expect(p.toString()).toBe("x=1");
  });
});

test("pending first, every decided status in the history, server order kept", () => {
  const list = [
    summary({ id: 5 }),
    summary({ id: 4, status: "accepted" }),
    summary({ id: 3, status: "superseded", superseded_by: 5 }),
    summary({ id: 2 }),
    summary({ id: 1, status: "rejected" }),
  ];
  const { pending, history } = splitSuggestions(list);
  expect(pending.map((s) => s.id)).toEqual([5, 2]);
  expect(history.map((s) => s.id)).toEqual([4, 3, 1]);
});

test("frontmatter is split from the body", () => {
  expect(splitFrontmatter("---\nname: a\ndescription: b\n---\n\n# Steps\n1.")).toEqual({
    frontmatter: "name: a\ndescription: b",
    body: "\n# Steps\n1.",
  });
  expect(splitFrontmatter("---\r\nname: a\r\n---\r\nbody")).toEqual({ frontmatter: "name: a", body: "body" });
  expect(splitFrontmatter("# no frontmatter")).toEqual({ frontmatter: null, body: "# no frontmatter" });
  expect(splitFrontmatter("---\nnever closed")).toEqual({ frontmatter: null, body: "---\nnever closed" });
});

test("one line, cut with an ellipsis", () => {
  expect(oneLine("a\n  b\tc", 10)).toBe("a b c");
  expect(oneLine("abcdefghij", 5)).toBe("abcd…");
});

test("the live skill note names a changed or missing skill", () => {
  const live = (over: Partial<LiveSkill>): LiveSkill => ({ path: "/p", text: "x", state: "unchanged", problem: null, ...over });
  expect(liveSkillNote(live({})).kind).toBe("ok");
  expect(liveSkillNote(live({ state: "changed" }))).toMatchObject({ kind: "down" });
  expect(liveSkillNote(live({ state: "changed" })).text).toContain("changed after this suggestion was written");
  expect(liveSkillNote(live({ state: "missing", text: null, problem: "there is no personal skill `a` to patch" })).text).toContain(
    "there is no personal skill",
  );
});

test("confirmation and result texts", () => {
  expect(acceptConfirmText({ kind: "create", name: "a-b", retires: [] })).toContain(".nucleus/.claude/skills/a-b/SKILL.md");
  expect(acceptConfirmText({ kind: "patch", name: "a-b", retires: [] })).toContain("Replace the private skill a-b");
  expect(acceptConfirmText({ kind: "patch", name: "a-b", retires: ["c-d", "e-f"] })).toContain(
    "Accept also moves the 2 merged skills c-d, e-f to .nucleus/.claude/skills/.archive/ (nothing is deleted).",
  );
  const resp = (git: SuggestionAcceptResp["git"], superseded: number[] = []): SuggestionAcceptResp => ({
    accepted: { id: 7, name: "a-b", path: "/ws/.nucleus/.claude/skills/a-b/SKILL.md", superseded, retired: [] },
    git,
  });
  const retired = resp({ status: "committed", sha: "0123456789abcdef" });
  retired.accepted.retired = [{ name: "c-d", archived_as: "c-d-20260101T000000" }];
  expect(describeAccept(retired).lines).toContain("retired c-d: moved to .archive/c-d-20260101T000000");
  expect(describeAccept(resp({ status: "committed", sha: "0123456789abcdef" }, [9]))).toEqual({
    kind: "ok",
    lines: [
      "accepted #7: wrote .nucleus/.claude/skills/a-b/SKILL.md",
      "git: committed 0123456 in .nucleus",
      "superseded the other pending suggestions for a-b: #9",
    ],
  });
  expect(describeAccept(resp({ status: "failed", error: "boom" })).kind).toBe("warn");
});

test("a refusal's head line leaves out the problems listed below it", () => {
  const issues = ["missing `# failure modes`", "description is empty"];
  expect(refusalHead(`the draft is not valid: ${issues.join("; ")}`, issues)).toBe("the draft is not valid");
  expect(refusalHead("suggestion #3 is rejected, not pending", [])).toBe("suggestion #3 is rejected, not pending");
  expect(refusalHead("other text", issues)).toBe("other text");
});
