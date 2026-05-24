// Reminders admin API — wraps the same `reminders::store` helpers
// the CLI uses (one source of truth per ADR-015).

import { jsonGet, jsonPost, qs } from "./client";

export type ReminderChannel = {
  reminder_id: number;
  channel: string;
  status: string;
  attempts: number;
  last_error: string | null;
  last_attempt_at: string | null;
};

export type ReminderStatus =
  | "active"
  | "pending"
  | "paused"
  | "fired"
  | "cancelled";

export type ReminderView = {
  id: number;
  title: string | null;
  body: string;
  cron: string;
  one_shot: boolean;
  status: ReminderStatus;
  next_fire_at: string | null;
  last_fired_at: string | null;
  paused_until: string | null;
  created_at: string;
  created_by: "user" | "system" | string;
  system_prompt: string | null;
  channels: ReminderChannel[];
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
export type RecentFire = {
  id: number;
  reminder_id: number;
  fired_at: string;
  channel: string;
  /** SQLite-style boolean. 1 = success, 0 = failure (error populated). */
  success: number;
  msg_id: string | null;
  error: string | null;
  reminder_title: string | null;
  reminder_body: string | null;
  /** 1 if the source reminder has a system_prompt (skill-fire). */
  is_skill_fire: number;
};

export const listReminderHistory = () => jsonGet<RecentFire[]>("/reminders/api/history");
