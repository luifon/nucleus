// Sessions API — tmux inspector for the long-lived `nucleus-*`
// sessions that host the bot Claude sessions (Rule 4).
// Mirrors nucleus-dashboard/api/src/handlers/sessions.rs.

import { jsonGet } from "./client";

export type TmuxWindow = {
  index: number;
  name: string;
  /** Unix epoch seconds of last activity. */
  activity_unix: number;
  panes: number;
};

export type TmuxSession = {
  name: string;
  created_unix: number;
  activity_unix: number;
  /** 1 if a client is currently attached, 0 otherwise. */
  attached: number;
  windows: TmuxWindow[];
};

export const listSessions = () => jsonGet<TmuxSession[]>("/sessions/api/list");
