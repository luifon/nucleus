import { type ChatMessage } from "@/lib/api";

export default function MessageBubble({
  message,
  personaName,
}: {
  message: ChatMessage;
  personaName: string;
}) {
  const isUser = message.role === "user";
  return (
    <div className={`flex ${isUser ? "justify-end" : "justify-start"}`}>
      <div
        className={[
          "max-w-[80%] rounded px-3 py-2 text-sm leading-relaxed",
          isUser
            ? "border border-[var(--color-nucleus-accent)] bg-[color-mix(in_srgb,var(--color-nucleus-accent)_8%,var(--color-nucleus-surface))] text-[var(--color-nucleus-text)]"
            : "border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] text-[var(--color-nucleus-text)]",
        ].join(" ")}
      >
        <div className="mb-1 flex items-center gap-2 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)] opacity-70">
          <span>{isUser ? "you" : personaName}</span>
          <span>·</span>
          <span>{shortTime(message.ts)}</span>
        </div>
        <pre className="whitespace-pre-wrap font-[inherit] text-sm">{message.content}</pre>
      </div>
    </div>
  );
}

function shortTime(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });
}
