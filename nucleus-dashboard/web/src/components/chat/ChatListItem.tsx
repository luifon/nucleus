import { MessageSquare, Trash2 } from "lucide-react";
import { type Chat } from "@/lib/api";

export default function ChatListItem({
  chat,
  active,
  onSelect,
  onDelete,
}: {
  chat: Chat;
  active: boolean;
  onSelect: () => void;
  onDelete: () => void;
}) {
  const display = chat.title ?? `chat ${chat.id.slice(0, 8)}`;
  return (
    <div
      onClick={onSelect}
      className={[
        "group flex cursor-pointer items-center gap-2 rounded px-2 py-1.5 transition-colors",
        active
          ? "bg-[color-mix(in_srgb,var(--color-nucleus-accent)_15%,transparent)] text-[var(--color-nucleus-accent)]"
          : "text-[var(--color-nucleus-text)] hover:bg-[var(--color-nucleus-bg)]",
      ].join(" ")}
    >
      <MessageSquare size={12} strokeWidth={1.75} className="shrink-0" />
      <div className="min-w-0 flex-1">
        <div className="truncate text-sm">{display}</div>
        <div className="text-[10px] text-[var(--color-nucleus-faint)]">
          {relTime(chat.last_active)}
        </div>
      </div>
      <button
        onClick={(e) => {
          e.stopPropagation();
          if (confirm(`Delete chat "${display}"? This can't be undone.`)) {
            onDelete();
          }
        }}
        title="delete chat"
        className="hidden text-[var(--color-nucleus-faint)] hover:text-[var(--color-status-down)] group-hover:block"
      >
        <Trash2 size={11} strokeWidth={1.75} />
      </button>
    </div>
  );
}

function relTime(iso: string): string {
  const then = new Date(iso).getTime();
  if (!Number.isFinite(then)) return "";
  const sec = Math.floor((Date.now() - then) / 1000);
  if (sec < 60) return `${sec}s ago`;
  if (sec < 3600) return `${Math.floor(sec / 60)}m ago`;
  if (sec < 86400) return `${Math.floor(sec / 3600)}h ago`;
  return `${Math.floor(sec / 86400)}d ago`;
}
