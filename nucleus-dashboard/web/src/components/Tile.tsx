import { type ComponentType, type ReactNode } from "react";
import StatusPill, { type StatusKind } from "./StatusPill";

// Reusable tile — used on Home, will be reused on Dashboard once that
// surface lands. Keep this generic; surface-specific content goes in
// `children`.
export default function Tile({
  Icon,
  label,
  status,
  statusKind,
  children,
}: {
  Icon?: ComponentType<{ size?: number; strokeWidth?: number; className?: string }>;
  label: string;
  status?: string;
  statusKind?: StatusKind;
  children?: ReactNode;
}) {
  return (
    <div className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] p-4">
      <div className="mb-3 flex items-center gap-2">
        {Icon && (
          <Icon size={14} strokeWidth={1.75} className="text-[var(--color-nucleus-accent)]" />
        )}
        <div className="flex-1 text-sm">{label}</div>
        {status && statusKind && <StatusPill kind={statusKind}>{status}</StatusPill>}
      </div>
      {children}
    </div>
  );
}
