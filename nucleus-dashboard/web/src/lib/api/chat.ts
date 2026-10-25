// Wire types are ts-rs-generated from the Rust structs (./generated/).

import { jsonGet, jsonPost } from "./client";
import type { MessageRow } from "./generated/MessageRow";
import type { ChatDetail as ChatDetailWire } from "./generated/ChatDetail";
import type { PendingTurn } from "./generated/PendingTurn";
import type { ChatRow as Chat } from "./generated/ChatRow";
import type { CreatedChat } from "./generated/CreatedChat";
import type { ChatInfo } from "./generated/ChatInfo";

export type { ChatRow as Chat } from "./generated/ChatRow";
export type { CreatedChat } from "./generated/CreatedChat";
export type { ChatInfo } from "./generated/ChatInfo";
export type { PendingTurn } from "./generated/PendingTurn";

/** UI-layer refinement: the wire shape (generated MessageRow) carries
 *  `role: string`; only these two values are ever written by the
 *  handler, so the dashboard narrows it for rendering. */
export type ChatRole = "user" | "assistant";

/** Wire shape is generated; `role` narrowing is a UI-layer refinement. */
export type ChatMessage = Omit<MessageRow, "role"> & {
  role: ChatRole;
};

/** Wire shape is generated; `messages` carry the UI-narrowed ChatMessage. */
export type ChatDetail = Omit<ChatDetailWire, "messages"> & {
  messages: ChatMessage[];
};

export const getChatInfo = () => jsonGet<ChatInfo>("/chat/api/info");
export const listChats = () => jsonGet<Chat[]>("/chat/api/chats");
export const createChat = () =>
  jsonPost<CreatedChat, Record<string, never>>("/chat/api/chats", {});
export const getChat = (id: string) => jsonGet<ChatDetail>(`/chat/api/chats/${encodeURIComponent(id)}`);
export const deleteChat = (id: string) =>
  fetch(`/chat/api/chats/${encodeURIComponent(id)}`, { method: "DELETE" }).then(async (r) => {
    if (!r.ok) throw new Error(`delete chat → ${r.status}`);
    return r.json() as Promise<{ ok: boolean; deleted: string }>;
  });
/** Starts a turn and returns at once (202); the reply arrives through
 *  `getChat`, whose `turn` stays "running" until it is stored. */
export const sendMessage = (id: string, message: string) =>
  jsonPost<PendingTurn, { message: string }>(
    `/chat/api/chats/${encodeURIComponent(id)}/messages`,
    { message },
  );
