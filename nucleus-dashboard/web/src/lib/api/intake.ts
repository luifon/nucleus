// Intake API — the issue pipeline's items (ADR-036), served by
// nucleus-dashboard/api/src/handlers/intake.rs under /intake/api.
// Wire types are ts-rs-generated from the Rust structs (./generated/).

import { jsonGet, jsonPost, qs } from "./client";
import type { IntakeItem as IntakeItemWire } from "./generated/IntakeItem";
import type { IntakeMessage as IntakeMessageWire } from "./generated/IntakeMessage";
import type { IntakeDetail as IntakeDetailWire } from "./generated/IntakeDetail";
import type { IntakeReplyReq } from "./generated/IntakeReplyReq";
import type { IntakeApprovePlanReq } from "./generated/IntakeApprovePlanReq";
import type { IntakeApproveCommentReq } from "./generated/IntakeApproveCommentReq";
import type { IntakeItemReq } from "./generated/IntakeItemReq";
import type { Task } from "./tasks";

export type { IntakeEval } from "./generated/IntakeEval";
export type { IntakeEvent } from "./generated/IntakeEvent";
export type { IntakeTransition } from "./generated/IntakeTransition";

/** UI-layer refinement of IntakeItem.stage (the values core/src/intake/stage.rs writes). */
export type IntakeStage =
  | "queued"
  | "eval"
  | "refinement"
  | "implementation"
  | "pr"
  | "review"
  | "closed"
  | "failed"
  | "cancelled";

/** UI-layer refinement of IntakeItem.comment_state. */
export type CommentState = "none" | "proposed" | "approved" | "posted" | "skipped";

/** UI-layer refinement of IntakeItem.surface. */
export type ThreadSurface = "none" | "pending" | "group" | "dm";

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

export type IntakeDetail = Omit<IntakeDetailWire, "item" | "messages" | "tasks"> & {
  item: IntakeItem;
  messages: IntakeMessage[];
  tasks: Task[];
};

/** Newest first. `all: false` leaves out closed and cancelled items. */
export const listIntakeItems = (opts: { all?: boolean } = {}, signal?: AbortSignal) =>
  jsonGet<IntakeItem[]>(`/intake/api/list${qs({ all: opts.all ?? true })}`, signal);

export const getIntakeDetail = (id: number, signal?: AbortSignal) =>
  jsonGet<IntakeDetail>(`/intake/api/detail${qs({ id })}`, signal);

// Every action answers 409 with `{ error }` when the pipeline refuses it
// (wrong stage, a newer plan, the agent still answering); ApiError carries
// that message.

export const replyToItem = (id: number, text: string) =>
  jsonPost<IntakeItem, IntakeReplyReq>("/intake/api/reply", { id, text });

export const approvePlan = (id: number, version: number) =>
  jsonPost<IntakeItem, IntakeApprovePlanReq>("/intake/api/approve-plan", { id, version });

export const approveComment = (id: number, text?: string) =>
  jsonPost<IntakeItem, IntakeApproveCommentReq>("/intake/api/approve-comment", text === undefined ? { id } : { id, text });

export const skipComment = (id: number) => jsonPost<IntakeItem, IntakeItemReq>("/intake/api/skip-comment", { id });

export const cancelItem = (id: number) => jsonPost<IntakeItem, IntakeItemReq>("/intake/api/cancel", { id });

export const retryItem = (id: number) => jsonPost<IntakeItem, IntakeItemReq>("/intake/api/retry", { id });
