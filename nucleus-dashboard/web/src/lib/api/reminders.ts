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
