// Work API — the issue pipeline's items (ADR-036), served by
// nucleus-dashboard/api/src/handlers/work.rs under /work/api.
// Wire types are ts-rs-generated from the Rust structs (./generated/).

import { jsonGet, jsonPost, qs } from "./client";
import type { WorkItem as WorkItemWire } from "./generated/WorkItem";
import type { WorkMessage as WorkMessageWire } from "./generated/WorkMessage";
import type { WorkDetail as WorkDetailWire } from "./generated/WorkDetail";
import type { WorkReplyReq } from "./generated/WorkReplyReq";
import type { WorkReplyKind } from "./generated/WorkReplyKind";
import type { WorkReplyResult as WorkReplyResultWire } from "./generated/WorkReplyResult";
import type { WorkApprovePlanReq } from "./generated/WorkApprovePlanReq";
import type { WorkItemReq } from "./generated/WorkItemReq";
import type { WorkReleaseReq } from "./generated/WorkReleaseReq";
import type { WorkAnswerReq } from "./generated/WorkAnswerReq";
import type { WorkQuestion as WorkQuestionWire } from "./generated/WorkQuestion";
import type { Task } from "./tasks";

export type { WorkEval } from "./generated/WorkEval";
export type { WorkHiddenFinding } from "./generated/WorkHiddenFinding";
export type { WorkHiddenSource } from "./generated/WorkHiddenSource";
export type { WorkEvent } from "./generated/WorkEvent";
export type { WorkTransition } from "./generated/WorkTransition";
export type { WorkPlanVersion } from "./generated/WorkPlanVersion";
export type { WorkReplyKind } from "./generated/WorkReplyKind";

/** UI-layer refinement of WorkItem.stage (the values core/src/work/stage.rs writes). */
export type WorkStage =
  | "queued"
  | "eval"
  | "refinement"
  | "implementation"
  | "pr"
  | "in_review"
  | "merged"
  | "not_merged"
  | "closed"
  | "failed"
  | "cancelled"
  | "stale"
  | "blocked"
  | "held";

/** UI-layer refinement of WorkItem.comment_state: the pull request link
 *  on the issue (`skipped`: the event's source has no reply channel). */
export type CommentState = "none" | "posted" | "skipped";

/** UI-layer refinement of WorkItem.surface: the item's WhatsApp thread
 *  is the operator's DM once it has one. */
export type ThreadSurface = "none" | "dm";

export type WorkItem = Omit<WorkItemWire, "stage" | "comment_state" | "surface"> & {
  stage: WorkStage;
  comment_state: CommentState;
  surface: ThreadSurface;
};

/** UI-layer refinements of WorkMessage.author / via. */
export type WorkMessage = Omit<WorkMessageWire, "author" | "via"> & {
  author: "operator" | "agent" | "nucleus";
  via: "whatsapp" | "dashboard" | "cli" | "pipeline" | "whatsapp-session";
};

/** UI-layer refinement of the decisions (core/src/work/decide.rs). */
export type WorkDecision = "approve_plan" | "release" | "cancel";

/** The confirmation question open on an item page. */
export type WorkQuestion = Omit<WorkQuestionWire, "decision"> & { decision: WorkDecision };

/** `plans` holds every accepted plan version, oldest first; `question` is
 *  the confirmation question open on the page, if any. */
export type WorkDetail = Omit<WorkDetailWire, "item" | "messages" | "tasks" | "question"> & {
  item: WorkItem;
  messages: WorkMessage[];
  tasks: Task[];
  question: WorkQuestion | null;
};

/** UI-layer refinement of WorkReplyResult.outcome. */
export type WorkReplyOutcome = "discussion" | "decision" | "question" | "unclear" | "declined" | "refused";

/** What text typed on the item page, or a Yes / No on the board, did. */
export type WorkReplyResult = Omit<WorkReplyResultWire, "item" | "outcome" | "decision"> & {
  item: WorkItem;
  outcome: WorkReplyOutcome;
  decision: WorkDecision | null;
};

/** Newest first. `all: false` leaves out closed and cancelled items, and
 *  stale items a newer item of the same issue replaced. */
export const listWorkItems = (opts: { all?: boolean } = {}, signal?: AbortSignal) =>
  jsonGet<WorkItem[]>(`/work/api/list${qs({ all: opts.all ?? true })}`, signal);

export const getWorkDetail = (id: number, signal?: AbortSignal) =>
  jsonGet<WorkDetail>(`/work/api/detail${qs({ id })}`, signal);

// Every decision answers 409 with `{ error }` when the pipeline refuses it
// (wrong stage, a newer plan, the agent still answering); ApiError carries
// that message.

/** Text typed on the item page. While the item waits for the operator the
 *  server reads it like his WhatsApp messages (a decision, a confirmation
 *  question, discussion, or unclear); otherwise it is discussion. A canvas
 *  answer (`kind: "canvas"`, a click on a question the agent asked) is
 *  always discussion. 409 for an empty or too-long message, or when the
 *  message could not be read. */
export const replyToItem = (id: number, text: string, kind: WorkReplyKind = "text") =>
  jsonPost<WorkReplyResult, WorkReplyReq>("/work/api/reply", { id, text, kind });

/** Yes or No to the page's open confirmation question (`question`: its
 *  id). 409 when the question is no longer open. */
export const answerQuestion = (id: number, question: number, yes: boolean) =>
  jsonPost<WorkReplyResult, WorkAnswerReq>("/work/api/answer", { id, question, yes });

export const approvePlan = (id: number, version: number) =>
  jsonPost<WorkItem, WorkApprovePlanReq>("/work/api/approve-plan", { id, version });

export const cancelItem = (id: number) => jsonPost<WorkItem, WorkItemReq>("/work/api/cancel", { id });

export const retryItem = (id: number) => jsonPost<WorkItem, WorkItemReq>("/work/api/retry", { id });

/** Continue a held item with the hidden content the detail lists. `hold`
 *  is the fingerprint (`hold_hash`) the panel rendered: refused (409) when
 *  the item was held again since; refused, and the item goes stale, when
 *  the issue changed since. */
export const releaseItem = (id: number, hold: string) =>
  jsonPost<WorkItem, WorkReleaseReq>("/work/api/release", { id, hold });
