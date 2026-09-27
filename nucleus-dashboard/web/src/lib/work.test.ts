import { describe, expect, test } from "vitest";
import { ApiError } from "@/lib/api/client";
import type { WorkItem } from "@/lib/api";
import { parseResponses } from "@/lib/canvas";
import { fixtureItem, fixtureMessage, fixtureReplyResult } from "./work.fixtures";
import {
  boardFor,
  boardKey,
  cancelStep,
  canvasAnswerText,
  composerHint,
  moveHighlight,
  questionStep,
  replyNotice,
  threadAnsweredIds,
  threadBlocks,
  authorLabel,
  canApprovePlan,
  canApproveShown,
  canCancelItem,
  canCompose,
  composerPlaceholder,
  enterSends,
  itemFromSearch,
  planStatus,
  planVersions,
  sendReply,
  canRelease,
  canReply,
  findingKindLabel,
  findingPlace,
  holdCode,
  markRanges,
  canRetry,
  isOpenItem,
  isWorking,
  stageKind,
  surfaceLabel,
  threadOrder,
  waitingOn,
} from "./work";

const item = fixtureItem;

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
    expect(canReply(item({ stage: "pr" }))).toBe(false);
  });
  test("nothing waits for the operator once the pull request stage starts", () => {
    expect(waitingOn(item({ stage: "pr", pr_url: "https://example.invalid/pull/1" }))).toBeNull();
    expect(waitingOn(item({ stage: "closed", comment_state: "posted" }))).toBeNull();
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
    expect(isWorking(item({ stage: "pr" }))).toBe(true);
    expect(isWorking(item({ stage: "closed" }))).toBe(false);
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

describe("item page", () => {
  test("the deep link parameter names a positive item number", () => {
    const at = (q: string) => itemFromSearch(new URLSearchParams(q));
    expect(at("item=12")).toBe(12);
    expect(at("item= 7 ")).toBe(7);
    expect(at("")).toBeNull();
    expect(at("item=0")).toBeNull();
    expect(at("item=-3")).toBeNull();
    expect(at("item=4x")).toBeNull();
    expect(at("item=99999999999999999999")).toBeNull();
  });

  test("plan versions come from `plans`, oldest first", () => {
    const plans = [
      { version: 2, text: "two", at: "2026-09-24T11:00:00.000Z" },
      { version: 1, text: "one", at: "2026-09-24T10:00:00.000Z" },
    ];
    expect(planVersions({ plans }).map((p) => p.version)).toEqual([1, 2]);
    expect(planVersions({ plans: [] })).toEqual([]);
  });

  test("plan status and the approve button follow the version on screen", () => {
    const it = item({ plan_version: 3 });
    expect(planStatus(3, it)).toBe("proposed");
    expect(planStatus(2, it)).toBe("replaced");
    expect(planStatus(3, item({ plan_version: 3, approved_version: 3 }))).toBe("approved");
    expect(canApproveShown(it, 3)).toBe(true);
    expect(canApproveShown(it, 2)).toBe(false);
    expect(canApproveShown(it, null)).toBe(false);
    expect(canApproveShown(item({ plan_version: 3, current_task_id: "t" }), 3)).toBe(false);
  });

  test("the composer is open until the item is finished", () => {
    expect(canCompose(item())).toBe(true);
    expect(canCompose(item({ stage: "implementation" }))).toBe(true);
    expect(canCompose(item({ stage: "closed" }))).toBe(false);
  });

  test("Enter sends on a desktop keyboard only", () => {
    expect(enterSends({ key: "Enter", shiftKey: false }, false)).toBe(true);
    expect(enterSends({ key: "Enter", shiftKey: true }, false)).toBe(false);
    expect(enterSends({ key: "Enter", shiftKey: false, isComposing: true }, false)).toBe(false);
    expect(enterSends({ key: "Enter", shiftKey: false }, true)).toBe(false);
    expect(enterSends({ key: "a", shiftKey: false }, false)).toBe(false);
  });

  test("the keyboard hint shows only where Enter sends", () => {
    expect(composerPlaceholder(false)).toContain("Enter sends");
    expect(composerPlaceholder(true)).not.toContain("Enter");
  });

  test("a reply that reaches the agent has no note", async () => {
    const calls: [number, string][] = [];
    const ok = await sendReply(4, "looks good", async (id, text) => {
      calls.push([id, text]);
      return fixtureReplyResult();
    });
    expect(calls).toEqual([[4, "looks good"]]);
    expect(ok).toEqual({ kind: "sent", item: item(), outcome: "discussion", note: null });
  });

  test("a reply saved outside refinement carries the server's note", async () => {
    const note = "Saved. Item #4 is in implementation; the agent does not read it now.";
    const saved = await sendReply(4, "x", async () =>
      fixtureReplyResult({ item: item({ stage: "implementation" }), reaches_agent: false, note }),
    );
    expect(saved).toEqual({ kind: "sent", item: item({ stage: "implementation" }), outcome: "discussion", note });
    const noNote = await sendReply(4, "x", async () => fixtureReplyResult({ reaches_agent: false, note: null }));
    expect(noNote.kind === "sent" && noNote.note).toBe("Saved in the thread. No agent reads it now.");
  });

  test("a canvas answer is sent marked as one; typed text as text", async () => {
    const kinds: (string | undefined)[] = [];
    const post = async (_id: number, _text: string, kind?: string) => {
      kinds.push(kind);
      return fixtureReplyResult();
    };
    await sendReply(4, "<canvas-response v=\"1\" id=\"a\" type=\"decision\">{\"choice\":\"x\"}</canvas-response>", post, "canvas");
    await sendReply(4, "approve the plan", post);
    expect(kinds).toEqual(["canvas", "text"]);
  });

  test("the composer reports what Nucleus did with a message", () => {
    const r = (over: Parameters<typeof fixtureReplyResult>[0]) => replyNotice(fixtureReplyResult(over));
    expect(r({ outcome: "decision", decision: "approve_plan", reaches_agent: false })).toBe("Plan approved. Implementation starts.");
    expect(r({ outcome: "decision", decision: "cancel", reaches_agent: false })).toBe("Item cancelled.");
    expect(r({ outcome: "question", reaches_agent: false, note: "Cancel item #4? Answer yes or no." })).toMatch(/Yes or No on the board/);
    const options = "I did not understand which decision you mean.\n\nWhat each item is waiting for:";
    expect(r({ outcome: "unclear", reaches_agent: false, note: options })).toBe(options);
    expect(r({ outcome: "refused", reaches_agent: false, note: "Plan v1 is not the latest plan." })).toBe("Plan v1 is not the latest plan.");
    expect(r({ outcome: "declined", reaches_agent: false, note: null })).toBe("Nothing was done.");
  });

  test("non-2xx answers are errors", async () => {
    const empty = await sendReply(4, " ", async () => {
      throw new ApiError("/work/api/reply", 409, "The message is empty.");
    });
    expect(empty).toEqual({ kind: "error", message: "The message is empty." });
    const broken = await sendReply(4, "x", async () => {
      throw new ApiError("/work/api/reply", 500, "database locked");
    });
    expect(broken).toEqual({ kind: "error", message: "database locked" });
  });

  test("the surface is the DM or nothing yet", () => {
    expect(surfaceLabel(item({ surface: "none" }))).toBe("not on WhatsApp yet");
  });
});

describe("decision board", () => {
  const keys = (b: ReturnType<typeof boardFor>) => (b.kind === "board" ? b.options.map((o) => o.key) : b.kind);

  test("refinement with a proposed plan: approve that version, continue discussing, cancel", () => {
    const b = boardFor(item({ plan_version: 2, plan_draft: "# Plan" }));
    expect(keys(b)).toEqual(["approve", "discuss", "cancel"]);
    expect(b.kind === "board" && b.options[0].label).toBe("Approve plan v2");
    expect(b.kind === "board" && b.options[1].label).toBe("Continue discussing");
    expect(b.kind === "board" && b.options[2].label).toBe("Cancel item");
  });

  test("refinement with no plan while the agent waits: the composer, no board", () => {
    expect(boardFor(item())).toEqual({ kind: "composer" });
  });

  test("held: release bound to the hold, continue discussing, cancel", () => {
    const b = boardFor(item({ stage: "held", hold_hash: "a1b2c3d4e5f6" }));
    expect(keys(b)).toEqual(["release", "discuss", "cancel"]);
    expect(b.kind === "board" && b.options[0].label).toBe("Release (hold a1b2c3)");
  });

  test("failed or blocked: retry, cancel", () => {
    for (const stage of ["failed", "blocked"] as const) {
      const b = boardFor(item({ stage, failed_stage: "pr" }));
      expect(keys(b)).toEqual(["retry", "cancel"]);
      expect(b.kind === "board" && b.title).toContain("in pr");
    }
  });

  test("while an agent works: the status line and Write a message", () => {
    const cases: [Partial<WorkItem>, string][] = [
      [{ stage: "eval" }, "evaluating"],
      [{ stage: "refinement", current_task_id: "t", plan_version: 2, plan_draft: "x" }, "writing a reply (plan v2"],
      [{ stage: "implementation" }, "implementing"],
      [{ stage: "pr" }, "draft pull request"],
      [{ stage: "queued" }, "preparing"],
    ];
    for (const [over, status] of cases) {
      const b = boardFor(item(over));
      expect(keys(b)).toEqual(["write"]);
      expect(b.kind === "board" && b.title).toContain(status);
    }
  });

  test("finished items show neither the board nor the composer", () => {
    for (const stage of ["closed", "cancelled", "stale", "merged", "not_merged"] as const) {
      expect(boardFor(item({ stage }))).toEqual({ kind: "closed" });
      expect(isOpenItem(stage)).toBe(false);
    }
  });

  test("an item in review waits for the PR review and stays open", () => {
    const b = boardFor(item({ stage: "in_review", pr_url: "https://example.invalid/acme/widget/pull/11" }));
    expect(b.kind === "board" && b.title).toContain("waits for your review");
    expect(isOpenItem("in_review")).toBe(true);
    expect(stageKind(item({ stage: "merged" }))).toBe("ok");
    expect(stageKind(item({ stage: "not_merged" }))).toBe("idle");
  });

  test("the board comes back when what it offers changes", () => {
    const a = boardKey(item({ plan_version: 1 }));
    expect(boardKey(item({ plan_version: 1 }))).toBe(a);
    expect(boardKey(item({ plan_version: 2 }))).not.toBe(a);
    expect(boardKey(item({ plan_version: 1, current_task_id: "t" }))).not.toBe(a);
    expect(boardKey(item({ stage: "implementation", plan_version: 1 }))).not.toBe(a);
  });

  test("steps: cancel asks once more; a server question binds what it names", () => {
    expect(cancelStep(4).title).toMatch(/^Cancel item #4\?/);
    expect(cancelStep(4).options.map((o) => o.key)).toEqual(["yes", "no"]);
    expect(questionStep(4, { decision: "approve_plan", plan_version: 3, hold_hash: null }).title).toMatch(/^Approve plan v3 of item #4\?/);
    expect(questionStep(4, { decision: "release", plan_version: null, hold_hash: "ffeedd001122" }).title).toContain("hold ffeedd");
    expect(questionStep(4, { decision: "cancel", plan_version: null, hold_hash: null }).options.map((o) => o.label)).toEqual(["Yes", "No"]);
  });

  test("arrow keys move the highlight and wrap; Home and End jump; other keys do nothing", () => {
    expect(moveHighlight(0, "ArrowDown", 3)).toBe(1);
    expect(moveHighlight(2, "ArrowDown", 3)).toBe(0);
    expect(moveHighlight(0, "ArrowUp", 3)).toBe(2);
    expect(moveHighlight(1, "ArrowLeft", 3)).toBe(0);
    expect(moveHighlight(1, "ArrowRight", 3)).toBe(2);
    expect(moveHighlight(1, "Home", 3)).toBe(0);
    expect(moveHighlight(0, "End", 3)).toBe(2);
    expect(moveHighlight(0, "Enter", 3)).toBeNull();
    expect(moveHighlight(0, "a", 3)).toBeNull();
    expect(moveHighlight(0, "ArrowDown", 0)).toBeNull();
  });

  test("the composer hint says what happens to a message in each stage", () => {
    expect(composerHint("refinement")).toBeNull();
    expect(composerHint("held")).toMatch(/decision in your own words/);
    expect(composerHint("implementation")).toContain("the agent reads replies only during refinement");
  });
});

describe("canvas questions in the thread", () => {
  const block = `<canvas v="1" type="decision" id="fmt" title="Output format">{"options":[{"key":"j","label":"JSON"},{"key":"y","label":"YAML"}]}</canvas>`;
  const agent = fixtureMessage(1, { author: "agent", body: `Which format?\n${block}` });
  const answer = fixtureMessage(2, { author: "operator", via: "dashboard", body: `<canvas-response v="1" id="fmt" type="decision">\n{"choice":"y"}\n</canvas-response>` });

  test("a block is answered once a later operator message carries its response", () => {
    expect(threadAnsweredIds([agent]).has("fmt")).toBe(false);
    expect(threadAnsweredIds([agent, answer]).has("fmt")).toBe(true);
    // An agent quoting the response does not answer it.
    expect(threadAnsweredIds([agent, { ...answer, author: "agent" }]).has("fmt")).toBe(false);
  });

  test("the operator's answer names the option label", () => {
    const blocks = threadBlocks([agent, answer]);
    expect([...blocks.keys()]).toEqual(["fmt"]);
    const [r] = parseResponses(answer.body);
    expect(canvasAnswerText(r, blocks.get("fmt"))).toBe("Output format: YAML");
    expect(canvasAnswerText(r, undefined)).toBe("fmt: y");
  });
});
