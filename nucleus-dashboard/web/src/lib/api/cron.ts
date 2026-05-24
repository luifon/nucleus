// Cron API — aggregated view of scheduled work.
// Mirrors `nucleus-dashboard/api/src/handlers/cron.rs`. Three sources
// stitched: launchctl, upcoming reminders, recent fire history.

import { jsonGet } from "./client";

export type LaunchdJob = {
  label: string;
  /** PID when the job is currently running; null for cron-style jobs
   *  that ran their command and exited. */
  pid: number | null;
  /** Last exit status. 0 = clean. Positive = error. Negative = signal
   *  (e.g. -9 SIGKILL, -15 SIGTERM) — common for restarted daemons,
   *  not necessarily an error condition. */
  last_exit: number | null;
};

export type UpcomingFire = {
  id: number;
  /** Operator-set short label (ADR-015). Preferred over body /
   *  system_prompt for display. Null when not set. */
  title: string | null;
  body: string | null;
  cron: string | null;
  one_shot: number;
  status: string;
  next_fire_at: string;
  last_fired_at: string | null;
  created_by: string;
  /** Non-null when this reminder fires a Claude session at the
   *  scheduled time (ADR-008 skill-fire). */
  system_prompt: string | null;
  /** Comma-joined channel list, e.g. `discord-home | whatsapp-dm`. */
  channels: string | null;
};

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

export const listLaunchdJobs = () => jsonGet<LaunchdJob[]>("/cron/api/launchd");
export const listUpcomingFires = () => jsonGet<UpcomingFire[]>("/cron/api/upcoming");
export const listRecentFires = () => jsonGet<RecentFire[]>("/cron/api/recent");
