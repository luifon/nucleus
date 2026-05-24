import { useEffect, useRef, useState, type ReactNode } from "react";
import { ChevronDown, Check } from "lucide-react";

export type Option = { value: string; label: string; meta?: ReactNode };

// Multi-select dropdown — button + popover with checkboxes.
// Click-outside closes. Used by NewsPage for source filtering; will be
// reused by Skills (operator/developer), Reminders (channel), etc.
export default function FilterDropdown({
  label,
  options,
  selected,
  onChange,
  allLabel = "all",
}: {
  label: string;
  options: Option[];
  selected: string[];
  onChange: (next: string[]) => void;
  allLabel?: string;
}) {
  const [open, setOpen] = useState(false);
  const wrap = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onDocClick = (e: MouseEvent) => {
      if (wrap.current && !wrap.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", onDocClick);
    return () => document.removeEventListener("mousedown", onDocClick);
  }, [open]);

  const summary =
    selected.length === 0
      ? allLabel
      : selected.length === options.length
        ? allLabel
        : `${selected.length} of ${options.length}`;

  const toggle = (v: string) => {
    const next = selected.includes(v) ? selected.filter((s) => s !== v) : [...selected, v];
    onChange(next);
  };

  return (
    <div ref={wrap} className="relative">
      <button
        onClick={() => setOpen((o) => !o)}
        className="flex items-center gap-2 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-2.5 py-1 text-xs text-[var(--color-nucleus-text)] hover:border-[var(--color-nucleus-accent)]"
      >
        <span className="text-[var(--color-nucleus-faint)]">{label}</span>
        <span>{summary}</span>
        <ChevronDown size={12} strokeWidth={1.75} className="text-[var(--color-nucleus-faint)]" />
      </button>
      {open && (
        <div className="absolute left-0 z-10 mt-1 max-h-72 w-64 overflow-y-auto rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] py-1 shadow-xl">
          <button
            onClick={() => onChange([])}
            className="block w-full px-3 py-1.5 text-left text-xs text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
          >
            ▸ {allLabel} (clear filter)
          </button>
          <div className="my-1 border-t border-[var(--color-nucleus-border)]" />
          {options.map((opt) => {
            const isSelected = selected.includes(opt.value);
            return (
              <button
                key={opt.value}
                onClick={() => toggle(opt.value)}
                className={`flex w-full items-center gap-2 px-3 py-1 text-left text-xs hover:bg-[var(--color-nucleus-bg)] ${
                  isSelected ? "text-[var(--color-nucleus-accent)]" : "text-[var(--color-nucleus-text)]"
                }`}
              >
                <span className="w-3">{isSelected && <Check size={11} strokeWidth={2} />}</span>
                <span className="flex-1 truncate">{opt.label}</span>
                {opt.meta && <span className="shrink-0">{opt.meta}</span>}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}
