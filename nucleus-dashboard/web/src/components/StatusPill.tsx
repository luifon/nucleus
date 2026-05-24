import { type ReactNode } from "react";

export type StatusKind = "ok" | "warn" | "down" | "idle";

const COLOR: Record<StatusKind, string> = {
  ok:   "text-[var(--color-status-ok)]",
  warn: "text-[var(--color-status-warn)]",
  down: "text-[var(--color-status-down)]",
  idle: "text-[var(--color-nucleus-faint)]",
};

// Bracketed-uppercase status pill — `[OK]`, `[DOWN]`, `[FIRING]`.
// Per ADR-015 §"Aesthetic guardrails" #4.
export default function StatusPill({
  kind,
  children,
}: {
  kind: StatusKind;
  children: ReactNode;
}) {
  return <span className={`text-xs ${COLOR[kind]}`}>[{children}]</span>;
}
