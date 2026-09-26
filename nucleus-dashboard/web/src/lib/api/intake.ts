// Intake API — the issue pipeline's items (ADR-036), served by
// nucleus-dashboard/api/src/handlers/intake.rs under /intake/api.
// Wire types are ts-rs-generated from the Rust structs (./generated/).

import { jsonGet, jsonPost, qs } from "./client";
import type { IntakeItem as IntakeItemWire } from "./generated/IntakeItem";
import type { IntakeMessage as IntakeMessageWire } from "./generated/IntakeMessage";
import type { IntakeDetail as IntakeDetailWire } from "./generated/IntakeDetail";
import type { IntakeReplyReq } from "./generated/IntakeReplyReq";
import type { IntakeReplyResult as IntakeReplyResultWire } from "./generated/IntakeReplyResult";
import type { IntakeApprovePlanReq } from "./generated/IntakeApprovePlanReq";
import type { IntakeItemReq } from "./generated/IntakeItemReq";
import type { IntakeReleaseReq } from "./generated/IntakeReleaseReq";
import type { IntakeAnswerReq } from "./generated/IntakeAnswerReq";
import type { IntakeQuestion as IntakeQuestionWire } from "./generated/IntakeQuestion";
import type { Task } from "./tasks";

export type { IntakeEval } from "./generated/IntakeEval";
export type { IntakeHiddenFinding } from "./generated/IntakeHiddenFinding";
export type { IntakeHiddenSource } from "./generated/IntakeHiddenSource";
export type { IntakeEvent } from "./generated/IntakeEvent";
export type { IntakeTransition } from "./generated/IntakeTransition";
export type { IntakePlanVersion } from "./generated/IntakePlanVersion";

/** UI-layer refinement of IntakeItem.stage (the values core/src/intake/stage.rs writes). */
export type IntakeStage =
  | "queued"
  | "eval"
  | "refinement"
  | "implementation"
  | "pr"
  | "closed"
  | "failed"
  | "cancelled"
  | "stale"
  | "blocked"
  | "held";

/** UI-layer refinement of IntakeItem.comment_state: the pull request link
 *  on the issue (`skipped`: the event's source has no reply channel). */
export type CommentState = "none" | "posted" | "skipped";

/** UI-layer refinement of IntakeItem.surface: the item's WhatsApp thread
 *  is the operator's DM once it has one. */
export type ThreadSurface = "none" | "dm";

export type IntakeItem = Omit<IntakeItemWire, "stage" | "comment_state" | "surface"> & {
  stage: IntakeStage;
  comment_state: CommentState;
  surface: ThreadSurface;
};

/** UI-layer refinements of IntakeMessage.author / via. */
export type IntakeMessage = Omit<IntakeMessageWire, "author" | "via"> & {
  author: "operator" | "agent" | "nucleus";
  via: "whatsapp" | "dashboard" | "cli" | "pipeline" | "whatsapp-session";
};

/** UI-layer refinement of the decisions (core/src/intake/decide.rs). */
export type IntakeDecision = "approve_plan" | "release" | "cancel";

/** The confirmation question open on an item page. */
export type IntakeQuestion = Omit<IntakeQuestionWire, "decision"> & { decision: IntakeDecision };

/** `plans` holds every accepted plan version, oldest first; `question` is
 *  the confirmation question open on the page, if any. */
export type IntakeDetail = Omit<IntakeDetailWire, "item" | "messages" | "tasks" | "question"> & {
  item: IntakeItem;
  messages: IntakeMessage[];
  tasks: Task[];
  question: IntakeQuestion | null;
};

/** UI-layer refinement of IntakeReplyResult.outcome. */
export type IntakeReplyOutcome = "discussion" | "decision" | "question" | "unclear" | "declined" | "refused";

/** What text typed on the item page, or a Yes / No on the board, did. */
export type IntakeReplyResult = Omit<IntakeReplyResultWire, "item" | "outcome" | "decision"> & {
  item: IntakeItem;
  outcome: IntakeReplyOutcome;
  decision: IntakeDecision | null;
};

/** Newest first. `all: false` leaves out closed and cancelled items, and
 *  stale items a newer item of the same issue replaced. */
export const listIntakeItems = (opts: { all?: boolean } = {}, signal?: AbortSignal) =>
  jsonGet<IntakeItem[]>(`/intake/api/list${qs({ all: opts.all ?? true })}`, signal);

export const getIntakeDetail = (id: number, signal?: AbortSignal) =>
  jsonGet<IntakeDetail>(`/intake/api/detail${qs({ id })}`, signal);

// Every decision answers 409 with `{ error }` when the pipeline refuses it
// (wrong stage, a newer plan, the agent still answering); ApiError carries
// that message.

/** Text typed on the item page. While the item waits for the operator the
 *  server reads it like his WhatsApp messages (a decision, a confirmation
 *  question, discussion, or unclear); otherwise, and for canvas responses,
 *  it is discussion. 409 for an empty or too-long message, or when the
 *  message could not be read. */
export const replyToItem = (id: number, text: string) =>
  jsonPost<IntakeReplyResult, IntakeReplyReq>("/intake/api/reply", { id, text });

/** Yes or No to the page's open confirmation question (`question`: its
 *  id). 409 when the question is no longer open. */
export const answerQuestion = (id: number, question: number, yes: boolean) =>
  jsonPost<IntakeReplyResult, IntakeAnswerReq>("/intake/api/answer", { id, question, yes });

export const approvePlan = (id: number, version: number) =>
  jsonPost<IntakeItem, IntakeApprovePlanReq>("/intake/api/approve-plan", { id, version });

export const cancelItem = (id: number) => jsonPost<IntakeItem, IntakeItemReq>("/intake/api/cancel", { id });

export const retryItem = (id: number) => jsonPost<IntakeItem, IntakeItemReq>("/intake/api/retry", { id });

/** Continue a held item with the hidden content the detail lists. `hold`
 *  is the fingerprint (`hold_hash`) the panel rendered: refused (409) when
 *  the item was held again since; refused, and the item goes stale, when
 *  the issue changed since. */
export const releaseItem = (id: number, hold: string) =>
  jsonPost<IntakeItem, IntakeReleaseReq>("/intake/api/release", { id, hold });
