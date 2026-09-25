// Tasks API — the background-task ledger and the WhatsApp turn log,
// served by nucleus-dashboard/api/src/handlers/tasks.rs under /tasks/api.
// Wire types are ts-rs-generated from the Rust structs (./generated/).

import { jsonGet, jsonPost, qs } from "./client";
import type { Task as TaskWire } from "./generated/Task";
import type { TaskEvent as TaskEventWire } from "./generated/TaskEvent";
import type { TaskLink } from "./generated/TaskLink";
import type { TurnRow as TurnRowWire } from "./generated/TurnRow";
import type { CancelTaskReq } from "./generated/CancelTaskReq";

export type { TaskLink } from "./generated/TaskLink";

/** UI-layer refinement: the wire shape (generated Task) carries
 *  `status: string`; this union narrows it to the lifecycle values the
 *  task ledger writes. */
export type TaskStatus =
  | "queued"
  | "running"
  | "done"
  | "failed"
  | "cancelled"
  | "interrupted";

/** Statuses in which the worker still owns the task and cancel applies. */
export const ACTIVE_TASK_STATUSES: readonly TaskStatus[] = ["queued", "running"];

/** Wire shape is generated; `status` narrowing is a UI-layer refinement. */
export type Task = Omit<TaskWire, "status"> & { status: TaskStatus };

/** UI-layer refinement of TaskEvent.kind (documented on the Rust struct). */
export type TaskEventKind =
  | "created"
  | "queued_for_slot"
  | "started"
  | "progress"
  | "background"
  | "runtime_guard"
  | "done"
  | "failed"
  | "cancelled"
  | "interrupted"
  | "delivery_queued"
  | "delivered"
  | "delivery_failed";

export type TaskEvent = Omit<TaskEventWire, "kind"> & { kind: TaskEventKind };

/** Detail envelope with the narrowed task and event types. */
export type TaskDetail = { task: Task; events: TaskEvent[]; links: TaskLink[] };

/** UI-layer refinements of TurnRow's text columns. */
export type TurnPool = "dm" | "group";
export type TurnKind = "operator" | "autonomous" | "context" | "foreign";
export type TurnStatus = "running" | "done" | "silent" | "failed" | "interrupted";

export type TurnRow = Omit<TurnRowWire, "pool" | "kind" | "status"> & {
  pool: TurnPool;
  kind: TurnKind;
  status: TurnStatus;
};

/** Newest first. `all: false` returns only queued and running tasks. */
export const listTasks = (opts: { all?: boolean } = {}, signal?: AbortSignal) =>
  jsonGet<Task[]>(`/tasks/api/list${qs({ all: opts.all ?? true })}`, signal);

export const getTaskDetail = (id: string, signal?: AbortSignal) =>
  jsonGet<TaskDetail>(`/tasks/api/detail${qs({ id })}`, signal);

/** Responds 409 with `{ error }` when the task already finished; the
 *  client surfaces that message through ApiError. */
export const cancelTask = (id: string) =>
  jsonPost<Task, CancelTaskReq>("/tasks/api/cancel", { id });

/** Recent WhatsApp conversational turns, newest first. */
export const listTurns = (signal?: AbortSignal) => jsonGet<TurnRow[]>("/tasks/api/turns", signal);
