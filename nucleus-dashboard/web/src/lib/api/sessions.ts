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

/** `tmux capture-pane -p` for the session's active pane (last N
 *  scrollback lines, default 20). Plain text. */
export const captureSessionPane = (session: string, lines = 20) =>
  fetch(`/sessions/api/capture?session=${encodeURIComponent(session)}&lines=${lines}`).then(
    async (r) => {
      if (!r.ok) throw new Error(`/sessions/api/capture → ${r.status}`);
      return r.text();
    },
  );
