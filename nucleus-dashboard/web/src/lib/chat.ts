// Pure helpers for the chat page. The server runs each turn and each title
// in a task of its own and reports them with the chat (`turn`, `titling`);
// the page derives what to show and whether to keep polling from that.

import type { ChatDetail, ChatMessage } from "@/lib/api";

/** id of the not-yet-stored user message of a running turn. */
export const PENDING_ID = -1;

/** Stored messages plus the running turn's message, until it is stored.
 *  The stored user row keeps the turn's timestamp, so a turn merged into a
 *  detail that already holds its rows is shown once. */
export function shownMessages(detail: ChatDetail | null): ChatMessage[] {
  if (!detail) return [];
  const turn = detail.turn;
  if (turn?.status !== "running") return detail.messages;
  if (detail.messages.some((m) => m.role === "user" && m.ts === turn.ts)) return detail.messages;
  return [
    ...detail.messages,
    { id: PENDING_ID, chat_id: detail.chat.id, role: "user", content: turn.message, ts: turn.ts },
  ];
}

/** True while the server still works on the chat: the page polls it. */
export function chatBusy(detail: ChatDetail | null): boolean {
  return detail?.turn?.status === "running" || detail?.titling === true;
}
