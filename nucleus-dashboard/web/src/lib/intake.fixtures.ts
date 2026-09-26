// Synthetic intake records for tests (the repo is public: no real values).

import type { IntakeDetail, IntakeEvent, IntakeItem, IntakeMessage, IntakeReplyResult } from "@/lib/api/intake";

export function fixtureItem(over: Partial<IntakeItem> = {}): IntakeItem {
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
    comment_state: "none",
    comment_url: null,
    comment_op: null,
    surface: "dm",
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

export function fixtureEvent(over: Partial<IntakeEvent> = {}): IntakeEvent {
  return {
    id: 1,
    source: "github",
    external_id: "acme/widget#12",
    project: "acme/widget",
    kind: "issue",
    title: "Fix typo",
    body: "The README says teh.",
    author: "reporter",
    labels: ["nucleus"],
    url: "https://example.invalid/acme/widget/issues/12",
    state: "open",
    created_at: "2026-09-24T08:00:00.000Z",
    updated_at: "2026-09-24T08:00:00.000Z",
    accepted: true,
    first_seen_at: "2026-09-24T08:00:00.000Z",
    last_seen_at: "2026-09-24T08:00:00.000Z",
    gate_note: null,
    ...over,
  };
}

export function fixtureMessage(id: number, over: Partial<IntakeMessage> = {}): IntakeMessage {
  return {
    id,
    item_id: 4,
    at: "2026-09-24T10:05:00.000Z",
    author: "agent",
    via: "pipeline",
    body: "message",
    pending_agent: 0,
    read_by_task: null,
    wa_state: null,
    ...over,
  };
}

export function fixtureDetail(item: IntakeItem = fixtureItem(), over: Partial<IntakeDetail> = {}): IntakeDetail {
  return {
    item,
    event: fixtureEvent(),
    eval: null,
    hidden: [],
    hidden_sources: [],
    plans: [],
    messages: [],
    tasks: [],
    transitions: [],
    ...over,
  };
}

export function fixturePlans(...texts: string[]): IntakeDetail["plans"] {
  return texts.map((text, k) => ({ version: k + 1, text, at: `2026-09-24T10:${String(10 + k * 10).padStart(2, "0")}:00.000Z` }));
}

export function fixtureReplyResult(over: Partial<IntakeReplyResult> = {}): IntakeReplyResult {
  return { item: fixtureItem(), reaches_agent: true, note: null, ...over };
}
