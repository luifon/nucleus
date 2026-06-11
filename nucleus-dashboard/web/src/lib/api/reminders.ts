// Reminders admin API — wraps the same `reminders::store` helpers
// the CLI uses (one source of truth per ADR-015).
// Wire types are ts-rs-generated from the Rust structs (./generated/).

import { jsonGet, jsonPost, qs } from "./client";
import type { ReminderView as ReminderViewWire } from "./generated/ReminderView";
import type { FireRow as RecentFire } from "./generated/FireRow";

export type { ChannelRow as ReminderChannel } from "./generated/ChannelRow";

/** UI-layer refinement: the wire shape (generated ReminderView) carries
 *  `status: string`; this union narrows it to the lifecycle values the
 *  store actually emits. */
export type ReminderStatus =
  | "active"
  | "pending"
  | "paused"
  | "fired"
  | "cancelled";

/** Wire shape is generated; `status` narrowing is a UI-layer refinement. */
export type ReminderView = Omit<ReminderViewWire, "status"> & {
  status: ReminderStatus;
};

export const listReminders = (opts: { includeFired?: boolean; includeCancelled?: boolean } = {}) =>
  jsonGet<ReminderView[]>(
    `/reminders/api/list${qs({
      include_fired: opts.includeFired,
      include_cancelled: opts.includeCancelled,
    })}`,
  );

export const pauseReminder = (id: number, until?: string) =>
  jsonPost<ReminderView, { id: number; until?: string }>("/reminders/api/pause", {
    id,
    ...(until ? { until } : {}),
  });

export const resumeReminder = (id: number) =>
  jsonPost<ReminderView, { id: number }>("/reminders/api/resume", { id });

export const cancelReminder = (id: number) =>
  jsonPost<ReminderView, { id: number }>("/reminders/api/cancel", { id });

export const setReminderTitle = (id: number, title: string | null) =>
  jsonPost<ReminderView, { id: number; title: string | null }>("/reminders/api/set-title", {
    id,
    title,
  });

// Fire-attempt audit log — folded in from the retired /cron surface (its one
// view /reminders lacked). "Upcoming" is just the active/pending rows of
// listReminders sorted by next_fire, so it needs no separate fetcher.
export type { FireRow as RecentFire } from "./generated/FireRow";

export const listReminderHistory = () => jsonGet<RecentFire[]>("/reminders/api/history");
