import { type ReactNode } from "react";

// A small tab in a view switcher inside a panel (plan / changes on a work
// item, rendered / raw / changes on a skill suggestion). The page-level tab
// strip is components/Tabs.

export default function ViewTab({
  active,
  disabled,
  onClick,
  children,
}: {
  active: boolean;
  disabled?: boolean;
  onClick: () => void;
  children: ReactNode;
}) {
  return (
    <button
      role="tab"
      aria-selected={active}
      disabled={disabled}
      onClick={onClick}
      className={[
        "rounded px-2 py-0.5 transition-colors disabled:opacity-40",
        active
          ? "bg-[color-mix(in_srgb,var(--color-nucleus-accent)_12%,transparent)] text-[var(--color-nucleus-accent)]"
          : "text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-text)]",
      ].join(" ")}
    >
      {children}
    </button>
  );
}
