import { type ReactNode } from "react";

// Generic horizontal tab strip. Pure presentational — owning component
// controls the active value via `value` + `onChange`. Used by Skills
// initially; reusable by Reminders, Vault, and any future surface that
// needs operator/system or active/history splits.

export type Tab<T extends string> = {
  value: T;
  label: ReactNode;
  /** Optional count badge — `5` shows `(5)` after the label. */
  count?: number | null;
};

export default function Tabs<T extends string>({
  tabs,
  value,
  onChange,
}: {
  tabs: Tab<T>[];
  value: T;
  onChange: (next: T) => void;
}) {
  return (
    <div className="mb-5 flex items-center gap-1 border-b border-[var(--color-nucleus-border)]">
      {tabs.map((t) => {
        const active = t.value === value;
        return (
          <button
            key={t.value}
            onClick={() => onChange(t.value)}
            className={[
              "relative -mb-px flex items-center gap-1.5 border-b-2 px-3 py-2 text-sm transition-colors",
              active
                ? "border-[var(--color-nucleus-accent)] text-[var(--color-nucleus-accent)]"
                : "border-transparent text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-text)]",
            ].join(" ")}
          >
            {t.label}
            {typeof t.count === "number" && (
              <span className="text-[10px] tabular-nums opacity-70">({t.count})</span>
            )}
          </button>
        );
      })}
    </div>
  );
}
