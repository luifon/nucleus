import { ChevronDown } from "lucide-react";
import { type ReactNode } from "react";

export type SelectOption<T extends string> = {
  value: T;
  label: ReactNode;
};

// Native <select> styled to the Nucleus palette. Used for
// single-choice filters (agent picker, channel picker). Native gives
// us keyboard support and accessibility for free, which the custom
// FilterDropdown would have to reinvent.

export default function Select<T extends string>({
  label,
  options,
  value,
  onChange,
}: {
  label: string;
  options: SelectOption<T>[];
  value: T;
  onChange: (next: T) => void;
}) {
  return (
    <label className="relative flex items-center gap-2 text-xs text-[var(--color-nucleus-faint)]">
      <span>{label}</span>
      <div className="relative">
        <select
          value={value}
          onChange={(e) => onChange(e.target.value as T)}
          className="appearance-none rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] py-1 pl-2 pr-7 text-xs text-[var(--color-nucleus-text)] hover:border-[var(--color-nucleus-accent)] focus:border-[var(--color-nucleus-accent)] focus:outline-none [color-scheme:dark]"
        >
          {options.map((opt) => (
            <option key={opt.value} value={opt.value} className="bg-[var(--color-nucleus-surface)]">
              {opt.label}
            </option>
          ))}
        </select>
        <ChevronDown
          size={12}
          strokeWidth={1.75}
          className="pointer-events-none absolute right-1.5 top-1/2 -translate-y-1/2 text-[var(--color-nucleus-faint)]"
        />
      </div>
    </label>
  );
}
