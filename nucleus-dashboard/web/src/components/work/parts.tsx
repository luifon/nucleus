import { type ReactNode } from "react";
import type { StatusKind } from "@/components/StatusPill";
import type { WorkItem } from "@/lib/api";
import { stageKind, stageLabel } from "@/lib/work";

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

const STAGE_COLOR: Record<StatusKind, string> = {
  ok: "text-[var(--color-status-ok)]",
  warn: "text-[var(--color-status-warn)]",
  down: "text-[var(--color-status-down)]",
  idle: "text-[var(--color-nucleus-faint)]",
};

/** An item's stage as a plain label ("In review"), coloured by its kind
 *  (`stageKind`): the operator reads words, not code names. */
export function StageBadge({ item }: { item: Pick<WorkItem, "stage" | "pr_url" | "current_task_id"> }) {
  return (
    <span data-stage={item.stage} className={`whitespace-nowrap rounded border border-current px-1.5 py-px text-[11px] ${STAGE_COLOR[stageKind(item)]}`}>
      {stageLabel(item.stage)}
    </span>
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
