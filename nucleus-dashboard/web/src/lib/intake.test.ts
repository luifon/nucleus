import { describe, expect, test } from "vitest";
import type { IntakeItem } from "@/lib/api/intake";
import {
  authorLabel,
  canApprovePlan,
  canCancelItem,
  canDecideComment,
  canRelease,
  canReply,
  findingKindLabel,
  findingPlace,
  holdCode,
  markRanges,
  canRetry,
  isWorking,
  stageKind,
  surfaceLabel,
  threadOrder,
  waitingOn,
} from "./intake";

function item(over: Partial<IntakeItem> = {}): IntakeItem {
  return {
    id: 4,
    event_id: 1,
    repo: "acme/widget",
    title: "Fix typo",
    stage: "refinement",
    failed_stage: null,
    error: null,
    classification: "complex",
    eval_json: null,
    plan_draft: null,
    plan_version: 0,
    approved_plan: null,
    approved_version: null,
    approved_at: null,
    approved_via: null,
    branch: null,
    worktree: null,
    base_ref: null,
    impl_summary: null,
    tests_status: null,
    tests_output: null,
    pr_url: null,
    comment_draft: null,
    comment_state: "none",
    comment_url: null,
    comment_op: null,
    surface: "group",
    group_requested_at: null,
    group_jid: null,
    group_closed_at: null,
    current_task_id: null,
    last_task_id: null,
    step_errors: 0,
    created_at: "2026-09-24T10:00:00.000Z",
    updated_at: "2026-09-24T10:00:00.000Z",
    closed_at: null,
    head_sha: null,
    rev_title: "Fix typo",
    rev_body: "body",
    revision_hash: null,
    gate_event_id: "labeled:1",
    label_event_id: "labeled:1",
    gate_actor: "maintainer",
    gate_at: "2026-09-24T09:00:00.000Z",
    stale_reason: null,
    base_sha: null,
    pushed_sha: null,
    hold_stage: null,
    hold_json: null,
    hold_hash: null,
    held_at: null,
    released_hash: null,
    released_at: null,
    released_via: null,
    ...over,
  };
}

describe("plan approval", () => {
  test("needs a plan and no running turn", () => {
    expect(canApprovePlan(item())).toBe(false);
    expect(canApprovePlan(item({ plan_version: 2 }))).toBe(true);
    expect(canApprovePlan(item({ plan_version: 2, current_task_id: "abc" }))).toBe(false);
    expect(canApprovePlan(item({ plan_version: 2, stage: "implementation" }))).toBe(false);
  });
  test("waiting text names the plan version", () => {
    expect(waitingOn(item({ plan_version: 3 }))).toBe("plan v3 waits for approval or a reply");
    expect(waitingOn(item())).toBe("the agent asked for a reply");
    expect(waitingOn(item({ current_task_id: "t" }))).toBeNull();
  });
});

describe("actions by stage", () => {
  test("reply only during refinement", () => {
    expect(canReply(item())).toBe(true);
    expect(canReply(item({ stage: "review" }))).toBe(false);
  });
  test("comment decisions only for a proposed comment in review", () => {
    expect(canDecideComment(item({ stage: "review", comment_state: "proposed" }))).toBe(true);
    expect(canDecideComment(item({ stage: "review", comment_state: "approved" }))).toBe(false);
    expect(waitingOn(item({ stage: "review", comment_state: "proposed" }))).toMatch(/comment/);
  });
  test("retry for failed items, cancel for open ones", () => {
    expect(canRetry(item({ stage: "failed" }))).toBe(true);
    expect(canRetry(item())).toBe(false);
    expect(canCancelItem(item({ stage: "failed" }))).toBe(true);
    expect(canCancelItem(item({ stage: "closed" }))).toBe(false);
    expect(canCancelItem(item({ stage: "cancelled" }))).toBe(false);
  });
});

describe("display", () => {
  test("stage colours", () => {
    expect(stageKind(item({ stage: "failed" }))).toBe("down");
    expect(stageKind(item({ stage: "closed", pr_url: "https://example.invalid/pull/1" }))).toBe("ok");
    expect(stageKind(item({ stage: "closed" }))).toBe("idle");
    expect(stageKind(item({ stage: "eval" }))).toBe("warn");
    expect(stageKind(item())).toBe("idle");
  });
  test("working items refresh", () => {
    expect(isWorking(item({ stage: "eval" }))).toBe(true);
    expect(isWorking(item())).toBe(false);
    expect(isWorking(item({ current_task_id: "t" }))).toBe(true);
    expect(isWorking(item({ stage: "review" }))).toBe(false);
  });
  test("labels", () => {
    expect(surfaceLabel(item({ surface: "dm" }))).toBe("WhatsApp DM, marked #4");
    expect(authorLabel({ author: "operator", via: "dashboard" })).toBe("you · dashboard");
    expect(authorLabel({ author: "nucleus", via: "pipeline" })).toBe("nucleus");
    const m = (id: number) => ({
      id,
      item_id: 4,
      at: "t",
      author: "agent" as const,
      via: "pipeline" as const,
      body: "b",
      pending_agent: 0,
      read_by_task: null,
      wa_state: null,
    });
    expect(threadOrder([m(3), m(1), m(2)]).map((x) => x.id)).toEqual([1, 2, 3]);
  });
});

describe("held items", () => {
  test("a held item can be released or cancelled, never retried", () => {
    const h = item({ stage: "held", hold_stage: "queued" });
    expect(canRelease(h)).toBe(true);
    expect(canCancelItem(h)).toBe(true);
    expect(canRetry(h)).toBe(false);
    expect(isWorking(h)).toBe(false);
    expect(stageKind(h)).toBe("warn");
    expect(waitingOn(h)).toMatch(/release or cancel/);
    expect(canRelease(item())).toBe(false);
  });
  test("findings read as place and kind", () => {
    expect(findingPlace({ location: "comment 55", line: 3, column: 7 })).toBe("comment 55 3:7");
    expect(findingKindLabel("html_comment")).toBe("HTML comment");
    expect(findingKindLabel("something_new")).toBe("something_new");
  });
  test("the raw source is split at the finding ranges, in code points", () => {
    const text = "a\u{1F600}<!-- x -->b";
    expect(markRanges(text, [{ start: 2, end: 12 }])).toEqual([
      { text: "a\u{1F600}", flagged: false },
      { text: "<!-- x -->", flagged: true },
      { text: "b", flagged: false },
    ]);
    expect(markRanges("abc", [])).toEqual([{ text: "abc", flagged: false }]);
    expect(markRanges("abcd", [{ start: 0, end: 2 }, { start: 1, end: 3 }]).map((p) => p.flagged)).toEqual([true, false]);
    expect(holdCode("a1b2c3d4")).toBe("a1b2c3");
  });
});

describe("stale and blocked items", () => {
  test("a blocked item can be retried or cancelled", () => {
    const b = item({ stage: "blocked", error: "the secret guard found pii-email in the branch or the pull request text" });
    expect(canRetry(b)).toBe(true);
    expect(canCancelItem(b)).toBe(true);
    expect(stageKind(b)).toBe("down");
    expect(waitingOn(b)).toMatch(/secret guard/);
  });
  test("a stale item is finished and says how to start again", () => {
    const s = item({ stage: "stale", stale_reason: "the issue title or body changed after the gate was satisfied" });
    expect(canRetry(s)).toBe(false);
    expect(canCancelItem(s)).toBe(false);
    expect(stageKind(s)).toBe("down");
    expect(waitingOn(s)).toMatch(/add the label again/);
  });
});
