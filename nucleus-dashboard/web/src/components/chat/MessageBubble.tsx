import { type ChatMessage } from "@/lib/api";
import { describeResponse, parseMessage, parseResponses } from "@/lib/canvas";
import CanvasBlock, { CanvasFallback } from "./CanvasBlock";

export default function MessageBubble({
  message,
  personaName,
  answeredIds,
  sending,
  onCanvasSubmit,
}: {
  message: ChatMessage;
  personaName: string;
  /** ids of canvas blocks already answered anywhere in this chat. */
  answeredIds: Set<string>;
  /** a send is in flight — canvas widgets must not double-submit. */
  sending: boolean;
  onCanvasSubmit: (message: string) => void;
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
        {isUser ? (
          <UserContent content={message.content} />
        ) : (
          <AssistantContent
            content={message.content}
            answeredIds={answeredIds}
            sending={sending}
            onCanvasSubmit={onCanvasSubmit}
          />
        )}
      </div>
    </div>
  );
}

/** Assistant text renders as interleaved plain-text + canvas segments. */
function AssistantContent({
  content,
  answeredIds,
  sending,
  onCanvasSubmit,
}: {
  content: string;
  answeredIds: Set<string>;
  sending: boolean;
  onCanvasSubmit: (message: string) => void;
}) {
  const segments = parseMessage(content);
  return (
    <>
      {segments.map((seg, i) => {
        if (seg.kind === "text") {
          return (
            <pre key={i} className="whitespace-pre-wrap font-[inherit] text-sm">
              {seg.text}
            </pre>
          );
        }
        if (seg.kind === "canvas-fallback") {
          return <CanvasFallback key={i} reason={seg.reason} raw={seg.raw} />;
        }
        return (
          <CanvasBlock
            key={seg.block.id}
            block={seg.block}
            answered={answeredIds.has(seg.block.id)}
            disabled={sending}
            onSubmit={onCanvasSubmit}
          />
        );
      })}
    </>
  );
}

/** User messages that are canvas responses render as compact chips; the
 *  raw XML-ish text stays in the transcript for the model only. */
function UserContent({ content }: { content: string }) {
  if (content.includes("<canvas-response")) {
    const responses = parseResponses(content);
    if (responses.length > 0) {
      return (
        <div className="flex flex-col gap-1">
          {responses.map((r, i) => (
            <span key={i} className="text-sm">
              ✔ {describeResponse(r)}
            </span>
          ))}
        </div>
      );
    }
  }
  return <pre className="whitespace-pre-wrap font-[inherit] text-sm">{content}</pre>;
}

function shortTime(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });
}
