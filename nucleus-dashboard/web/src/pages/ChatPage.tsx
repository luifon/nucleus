import { useEffect, useMemo, useRef, useState } from "react";
import { Plus, Send, MessageSquare, RefreshCw, ChevronLeft } from "lucide-react";
import {
  listChats,
  createChat,
  getChat,
  deleteChat,
  sendMessage,
  getChatInfo,
  type Chat,
  type ChatMessage,
} from "@/lib/api";
import { useFetch } from "@/lib/hooks";
import ChatListItem from "@/components/chat/ChatListItem";
import MessageBubble from "@/components/chat/MessageBubble";

const OPTIMISTIC_ID = -1;

export default function ChatPage() {
  const chats = useFetch(listChats);
  const info = useFetch(getChatInfo);
  const personaName = info.data?.persona_name ?? "assistant";
  const [activeId, setActiveId] = useState<string | null>(null);
  const [detail, setDetail] = useState<{ chat: Chat; messages: ChatMessage[] } | null>(null);
  const [loadingDetail, setLoadingDetail] = useState(false);
  const [draft, setDraft] = useState("");
  const [sending, setSending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const messagesEndRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    // Auto-open the first chat only on desktop (the side-by-side split). On
    // mobile, /chat opens to the full-width list so the conversation's "back"
    // control can return to it — otherwise this effect would immediately
    // re-select the first chat and the list would be unreachable.
    const isDesktop = window.matchMedia("(min-width: 768px)").matches;
    if (isDesktop && !activeId && chats.data && chats.data.length > 0) {
      setActiveId(chats.data[0].id);
    }
  }, [chats.data, activeId]);

  useEffect(() => {
    if (!activeId) {
      setDetail(null);
      return;
    }
    let cancelled = false;
    setLoadingDetail(true);
    setError(null);
    getChat(activeId)
      .then((d) => { if (!cancelled) setDetail(d); })
      .catch((e) => { if (!cancelled) setError(String(e)); })
      .finally(() => { if (!cancelled) setLoadingDetail(false); });
    return () => { cancelled = true; };
  }, [activeId]);

  useEffect(() => {
    messagesEndRef.current?.scrollIntoView({ behavior: "smooth" });
  }, [detail?.messages.length, sending]);

  const onCreate = async () => {
    try {
      const c = await createChat();
      await chats.refetch();
      setActiveId(c.id);
    } catch (e) {
      setError(String(e));
    }
  };

  const onDelete = async (id: string) => {
    try {
      await deleteChat(id);
      if (activeId === id) setActiveId(null);
      await chats.refetch();
    } catch (e) {
      setError(String(e));
    }
  };

  const onSend = async () => {
    if (!activeId || !draft.trim() || sending) return;
    const message = draft.trim();
    setDraft("");
    setSending(true);
    setError(null);

    // ask() can take many seconds; render the user message immediately
    // so the operator doesn't watch a blank box waiting for it to land.
    const optimisticTs = new Date().toISOString();
    setDetail((d) => d && {
      ...d,
      messages: [
        ...d.messages,
        { id: OPTIMISTIC_ID, chat_id: activeId, role: "user", content: message, ts: optimisticTs },
      ],
    });

    try {
      const resp = await sendMessage(activeId, message);
      setDetail((d) => d && {
        chat: {
          ...d.chat,
          title: resp.chat_title ?? d.chat.title,
          claude_session_id: resp.session_id,
          last_active: resp.assistant_message.ts,
        },
        messages: [
          ...d.messages.filter((m) => m.id !== OPTIMISTIC_ID),
          resp.user_message,
          resp.assistant_message,
        ],
      });
      await chats.refetch();
    } catch (e) {
      setError(String(e));
      // Restore the draft so the operator can edit and retry.
      setDetail((d) => d && { ...d, messages: d.messages.filter((m) => m.id !== OPTIMISTIC_ID) });
      setDraft(message);
    } finally {
      setSending(false);
    }
  };

  const onKeyDown = (e: React.KeyboardEvent<HTMLTextAreaElement>) => {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void onSend();
    }
  };

  const activeChat = useMemo(
    () => chats.data?.find((c) => c.id === activeId),
    [chats.data, activeId],
  );

  return (
    <div className="flex h-full overflow-hidden">
      <aside
        className={`${activeId ? "hidden md:flex" : "flex"} w-full shrink-0 flex-col border-r border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] md:w-64`}
      >
        <div className="flex items-center justify-between border-b border-[var(--color-nucleus-border)] px-3 py-2.5">
          <span className="text-xs uppercase tracking-widest text-[var(--color-nucleus-faint)] opacity-70">
            chats
          </span>
          <div className="flex items-center gap-1">
            <button
              onClick={chats.refetch}
              title="refresh"
              className="text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
            >
              <RefreshCw size={12} strokeWidth={1.75} />
            </button>
            <button
              onClick={onCreate}
              title="new chat"
              className="text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
            >
              <Plus size={14} strokeWidth={1.75} />
            </button>
          </div>
        </div>
        <div className="flex-1 overflow-y-auto p-2">
          {!chats.data ? (
            <div className="px-2 py-1 text-xs text-[var(--color-nucleus-faint)]">fetching…</div>
          ) : chats.data.length === 0 ? (
            <button
              onClick={onCreate}
              className="flex w-full items-center gap-2 rounded border border-dashed border-[var(--color-nucleus-border)] px-3 py-4 text-xs text-[var(--color-nucleus-faint)] hover:border-[var(--color-nucleus-accent)] hover:text-[var(--color-nucleus-accent)]"
            >
              <Plus size={12} strokeWidth={1.75} />
              start your first chat
            </button>
          ) : (
            <ul className="space-y-0.5">
              {chats.data.map((c) => (
                <li key={c.id}>
                  <ChatListItem
                    chat={c}
                    active={c.id === activeId}
                    onSelect={() => setActiveId(c.id)}
                    onDelete={() => onDelete(c.id)}
                  />
                </li>
              ))}
            </ul>
          )}
        </div>
      </aside>

      <main className={`${activeId ? "flex" : "hidden md:flex"} min-w-0 flex-1 flex-col overflow-hidden`}>
        {!activeId ? (
          <div className="flex flex-1 items-center justify-center text-sm text-[var(--color-nucleus-faint)]">
            <div className="flex items-center gap-2">
              <MessageSquare size={14} strokeWidth={1.75} />
              select or create a chat to start
            </div>
          </div>
        ) : (
          <>
            <header className="flex items-center justify-between gap-2 border-b border-[var(--color-nucleus-border)] px-4 py-2.5 md:px-5">
              <div className="flex min-w-0 items-center gap-2">
                <button
                  onClick={() => setActiveId(null)}
                  aria-label="back to chats"
                  className="shrink-0 text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)] md:hidden"
                >
                  <ChevronLeft size={18} strokeWidth={1.75} />
                </button>
                <div className="min-w-0 truncate text-sm text-[var(--color-nucleus-text)]">
                  {activeChat?.title ?? (activeId ? `chat ${activeId.slice(0, 8)}` : "")}
                </div>
              </div>
              <div className="shrink-0 text-[10px] text-[var(--color-nucleus-faint)]">
                {activeChat?.claude_session_id
                  ? `session ${activeChat.claude_session_id.slice(0, 8)}`
                  : "no session yet"}
              </div>
            </header>

            <div className="flex-1 overflow-y-auto p-5">
              {loadingDetail ? (
                <div className="text-xs text-[var(--color-nucleus-faint)]">loading messages…</div>
              ) : detail?.messages.length === 0 ? (
                <div className="text-xs text-[var(--color-nucleus-faint)]">
                  no messages yet. ask anything about your vault.
                </div>
              ) : (
                <ul className="space-y-3">
                  {detail?.messages.map((m, i) => (
                    <li key={m.id === OPTIMISTIC_ID ? `opt-${i}` : m.id}>
                      <MessageBubble message={m} personaName={personaName} />
                    </li>
                  ))}
                  {sending && (
                    <li>
                      <div className="flex justify-start">
                        <div className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-xs text-[var(--color-nucleus-faint)]">
                          <span className="opacity-70">{personaName} is thinking</span>
                          <span className="ml-1 inline-block animate-pulse">▸▸▸</span>
                        </div>
                      </div>
                    </li>
                  )}
                </ul>
              )}
              <div ref={messagesEndRef} />
            </div>

            {error && (
              <div className="border-t border-[var(--color-status-down)] bg-[color-mix(in_srgb,var(--color-status-down)_15%,var(--color-nucleus-surface))] px-5 py-2 text-xs text-[var(--color-status-down)]">
                {error}
              </div>
            )}

            <div className="border-t border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] p-3">
              <div className="flex items-end gap-2">
                <textarea
                  value={draft}
                  onChange={(e) => setDraft(e.target.value)}
                  onKeyDown={onKeyDown}
                  rows={2}
                  placeholder="ask…  (Enter to send · Shift+Enter for newline)"
                  disabled={sending}
                  className="flex-1 resize-y rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-3 py-2 text-sm text-[var(--color-nucleus-text)] focus:border-[var(--color-nucleus-accent)] focus:outline-none disabled:opacity-50"
                />
                <button
                  onClick={onSend}
                  disabled={sending || !draft.trim()}
                  className="flex items-center gap-1.5 rounded border border-[var(--color-nucleus-accent)] bg-[color-mix(in_srgb,var(--color-nucleus-accent)_12%,transparent)] px-3 py-2 text-sm text-[var(--color-nucleus-accent)] hover:bg-[color-mix(in_srgb,var(--color-nucleus-accent)_22%,transparent)] disabled:cursor-not-allowed disabled:opacity-40"
                >
                  <Send size={12} strokeWidth={1.75} />
                  send
                </button>
              </div>
            </div>
          </>
        )}
      </main>
    </div>
  );
}
