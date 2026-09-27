import { type ReactNode } from "react";

/**
 * The canvas box (ADR-012): accent border, tinted surface, and a header
 * line with the block's kind, its title and an "answered" marker. Shared
 * by the chat's canvas blocks and the work decision board. Titles are
 * rendered as text, never as HTML.
 */
export default function CanvasFrame({
  kind,
  title,
  answered = false,
  className = "",
  children,
}: {
  /** The header's label, e.g. "decision". */
  kind: string;
  title?: string;
  /** Dims the box and marks it answered. */
  answered?: boolean;
  className?: string;
  children: ReactNode;
}) {
  return (
    <div
      className={[
        "rounded border px-3 py-2",
        "border-[var(--color-nucleus-accent)]",
        "bg-[color-mix(in_srgb,var(--color-nucleus-accent)_5%,var(--color-nucleus-surface))]",
        answered ? "opacity-60" : "",
        className,
      ].join(" ")}
    >
      <div className="mb-2 flex items-center gap-2 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)]">
        <span className="shrink-0">⛶ {kind}</span>
        {title && <span className="min-w-0 normal-case tracking-normal text-[var(--color-nucleus-text)]">{title}</span>}
        {answered && <span className="ml-auto shrink-0">answered</span>}
      </div>
      {children}
    </div>
  );
}
