// Pure helpers for the Tasks page: duration and time formatting, status
// to pill-colour mapping, and the cancel availability rule. The server
// stays the authority for cancel; `canCancel` only hides the action when
// the listing already shows it would be refused.

import type { StatusKind } from "@/components/StatusPill";
import {
  ACTIVE_TASK_STATUSES,
  type Task,
  type TaskStatus,
  type TurnRow,
  type TurnStatus,
} from "@/lib/api/tasks";

/** Formats a millisecond span: `45s`, `3m05s`, `1h02m`, `2d03h`.
 *  Negative or non-finite input returns `—`. */
export function formatDuration(ms: number): string {
  if (!Number.isFinite(ms) || ms < 0) return "—";
  const total = Math.floor(ms / 1000);
  const pad = (n: number) => String(n).padStart(2, "0");
  if (total < 60) return `${total}s`;
  const minutes = Math.floor(total / 60);
  if (minutes < 60) return `${minutes}m${pad(total % 60)}s`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h${pad(minutes % 60)}m`;
  return `${Math.floor(hours / 24)}d${pad(hours % 24)}h`;
}

/** Duration between two ISO timestamps. `end` null means "still
 *  running": the span is measured to `now`. `start` null (never
 *  started) returns null so the caller can show a placeholder. */
export function spanBetween(start: string | null, end: string | null, now: number): string | null {
  if (!start) return null;
  const s = Date.parse(start);
  if (Number.isNaN(s)) return null;
  const e = end ? Date.parse(end) : now;
  if (Number.isNaN(e)) return null;
  return formatDuration(e - s);
}

/** Task runtime: started → finished, or started → now while running. */
export function taskDuration(task: Pick<Task, "started_at" | "finished_at">, now: number): string | null {
  return spanBetween(task.started_at, task.finished_at, now);
}

/** First 8 characters of a task id, the length shown in the list. */
export function shortId(id: string): string {
  return id.slice(0, 8);
}

export function isActiveTask(status: TaskStatus): boolean {
  return ACTIVE_TASK_STATUSES.includes(status);
}

/** Cancel applies to queued and running tasks. It is an immediate
 *  transition to `cancelled` (ADR-033), so a cancelled task is no longer
 *  active and cannot be cancelled again. */
export function canCancel(task: Pick<Task, "status">): boolean {
  return isActiveTask(task.status);
}

export type DeliveryState = "delivered" | "given-up" | "queued" | null;

/** Where the result's delivery to the task's origin stands. `given-up`:
 *  the delivery is not attempted again because its outcome is unknown or
 *  it failed too often (ADR-033); the operator got one note. A delivery
 *  confirmed later (a late server acknowledgement) is `delivered`. */
export function deliveryState(
  task: Pick<Task, "delivered_at" | "delivery_failed_at" | "delivery_queued_at">,
): DeliveryState {
  if (task.delivered_at) return "delivered";
  if (task.delivery_failed_at) return "given-up";
  if (task.delivery_queued_at) return "queued";
  return null;
}

/** Amber for work in progress, green for success, red for failure,
 *  faint for states that need no attention. */
export function taskStatusKind(status: TaskStatus): StatusKind {
  switch (status) {
    case "running":
      return "warn";
    case "done":
      return "ok";
    case "failed":
    case "interrupted":
      return "down";
    case "queued":
    case "cancelled":
    default:
      return "idle";
  }
}

export function turnStatusKind(status: TurnStatus): StatusKind {
  switch (status) {
    case "running":
      return "warn";
    case "done":
      return "ok";
    case "failed":
    case "interrupted":
      return "down";
    case "silent":
    default:
      return "idle";
  }
}

/** Turns whose error text is shown under the row. */
export function turnShowsError(turn: Pick<TurnRow, "status" | "error">): boolean {
  return (turn.status === "failed" || turn.status === "interrupted") && !!turn.error;
}

/** `HH:MM` for today, `DD/MM HH:MM` otherwise (same format as the
 *  reminders history). Unparseable input is returned unchanged. */
export function shortTime(iso: string, now: Date = new Date()): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const sameDay =
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate();
  const time = d.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });
  if (sameDay) return time;
  const day = d.toLocaleDateString("en-GB", { day: "2-digit", month: "2-digit" });
  return `${day} ${time}`;
}

/** `HH:MM:SS` for progress-log lines, where several events share a minute. */
export function clockTime(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}
