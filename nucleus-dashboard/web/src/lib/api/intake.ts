// Intake API — the issue pipeline's items (ADR-036), served by
// nucleus-dashboard/api/src/handlers/intake.rs under /intake/api.
// Wire types are ts-rs-generated from the Rust structs (./generated/).

import { jsonGet, jsonPost, qs } from "./client";
import type { IntakeItem as IntakeItemWire } from "./generated/IntakeItem";
import type { IntakeMessage as IntakeMessageWire } from "./generated/IntakeMessage";
import type { IntakeDetail as IntakeDetailWire } from "./generated/IntakeDetail";
import type { IntakeReplyReq } from "./generated/IntakeReplyReq";
import type { IntakeApprovePlanReq } from "./generated/IntakeApprovePlanReq";
import type { IntakeItemReq } from "./generated/IntakeItemReq";
import type { IntakeReleaseReq } from "./generated/IntakeReleaseReq";
import type { Task } from "./tasks";

export type { IntakeEval } from "./generated/IntakeEval";
export type { IntakeHiddenFinding } from "./generated/IntakeHiddenFinding";
export type { IntakeHiddenSource } from "./generated/IntakeHiddenSource";
export type { IntakeEvent } from "./generated/IntakeEvent";
export type { IntakeTransition } from "./generated/IntakeTransition";

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

// TEMPORARY — remove when the generated type includes plans. The backend
// change that adds `plans` to the detail response is built on a parallel
// branch; until it merges, IntakeDetailWire has no such field and older
// servers do not send it. Replace with the generated `IntakePlanVersion`
// and drop the optional `plans` below once `npm run generate:api` emits it.
/** One stored plan version of an item (oldest first in `plans`). */
export type IntakePlanVersion = { version: number; text: string; at: string };

export type IntakeDetail = Omit<IntakeDetailWire, "item" | "messages" | "tasks"> & {
  item: IntakeItem;
  messages: IntakeMessage[];
  tasks: Task[];
  /** TEMPORARY optional (remove when the generated type includes plans):
   *  every plan version, oldest first. Read as `detail.plans ?? []`. */
  plans?: IntakePlanVersion[];
};

/** Newest first. `all: false` leaves out closed and cancelled items, and
 *  stale items a newer item of the same issue replaced. */
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

export const cancelItem = (id: number) => jsonPost<IntakeItem, IntakeItemReq>("/intake/api/cancel", { id });

export const retryItem = (id: number) => jsonPost<IntakeItem, IntakeItemReq>("/intake/api/retry", { id });

/** Continue a held item with the hidden content the detail lists. `hold`
 *  is the fingerprint (`hold_hash`) the panel rendered: refused (409) when
 *  the item was held again since; refused, and the item goes stale, when
 *  the issue changed since. */
export const releaseItem = (id: number, hold: string) =>
  jsonPost<IntakeItem, IntakeReleaseReq>("/intake/api/release", { id, hold });
