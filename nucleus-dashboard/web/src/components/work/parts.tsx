import { type ReactNode } from "react";

// Small shared pieces of the work item page.

export function ActionButton({
  children,
  onClick,
  disabled,
  type = "button",
  tone = "accent",
}: {
  children: ReactNode;
  onClick?: () => void;
  disabled?: boolean;
  type?: "button" | "submit";
  /** Hover colour: accent for forward actions, down for destructive ones. */
  tone?: "accent" | "down";
}) {
  const hover =
    tone === "down"
      ? "hover:border-[var(--color-status-down)] hover:text-[var(--color-status-down)]"
      : "hover:border-[var(--color-nucleus-accent)] hover:text-[var(--color-nucleus-accent)]";
  return (
    <button
      type={type}
      onClick={onClick}
      disabled={disabled}
      className={`flex items-center gap-1.5 rounded border border-[var(--color-nucleus-border)] px-2 py-1 text-xs text-[var(--color-nucleus-faint)] transition-colors disabled:opacity-40 ${hover}`}
    >
      {children}
    </button>
  );
}

export function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <section>
      <div className="mb-1 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)] opacity-70">{label}</div>
      {children}
    </section>
  );
}

export function Pre({ children, tone = "text-[var(--color-nucleus-text)]" }: { children: string; tone?: string }) {
  return (
    <pre
      className={`max-h-80 overflow-auto whitespace-pre-wrap break-words rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-3 py-2 font-mono text-xs ${tone}`}
    >
      {children}
    </pre>
  );
}
