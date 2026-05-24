// Chat API — multi-chat against the Obsidian vault.
// Mirrors nucleus-dashboard/api/src/handlers/chat.rs.

import { jsonGet, jsonPost } from "./client";

export type Chat = {
  id: string;
  title: string | null;
  claude_session_id: string | null;
  created_at: string;
  last_active: string;
};

export type ChatMessage = {
  id: number;
  chat_id: string;
  role: "user" | "assistant";
  content: string;
  ts: string;
};

export type ChatDetail = {
  chat: Chat;
  messages: ChatMessage[];
};

export type CreatedChat = { id: string; created_at: string };

export type SendResp = {
  user_message: ChatMessage;
  assistant_message: ChatMessage;
  chat_title: string | null;
  session_id: string;
};

export type ChatInfo = {
  /** Display name from the chat persona's frontmatter (ADR-009).
   *  Used as the assistant-role label in the UI. */
  persona_name: string;
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
export const sendMessage = (id: string, message: string) =>
  jsonPost<SendResp, { message: string }>(
    `/chat/api/chats/${encodeURIComponent(id)}/messages`,
    { message },
  );
