import { Activity, Square, AlertOctagon, Zap } from "lucide-react";
import StatusPill, { type StatusKind } from "@/components/StatusPill";
import { type LaunchdJob } from "@/lib/api";

// Classify a launchd job's health based on PID + last exit code.
// Sources/conventions:
//   - PID set                → daemon, currently running       → OK
//   - PID null, exit  0      → cron-style ran cleanly          → OK (idle)
//   - PID null, exit negative→ signal-killed (-9 SIGKILL, -15 SIGTERM)
//                              common when launchd restarts a daemon —
//                              treat as warn (we want to notice,
//                              but it's not an outage by itself)
//   - PID null, exit positive→ command exited with error        → DOWN
function classify(job: LaunchdJob): { kind: StatusKind; label: string } {
  if (job.pid !== null) return { kind: "ok", label: "RUNNING" };
  if (job.last_exit === null) return { kind: "idle", label: "UNKNOWN" };
  if (job.last_exit === 0) return { kind: "ok", label: "IDLE" };
  if (job.last_exit < 0) return { kind: "warn", label: `SIG${-job.last_exit}` };
  return { kind: "down", label: `EXIT ${job.last_exit}` };
}

function iconFor(kind: StatusKind) {
  switch (kind) {
    case "ok":   return Activity;
    case "warn": return Zap;
    case "down": return AlertOctagon;
    case "idle": return Square;
  }
}

export default function LaunchdJobTile({ job }: { job: LaunchdJob }) {
  const { kind, label } = classify(job);
  const Icon = iconFor(kind);
  // Strip the `dev.nucleus.` prefix from the display label — the
  // operator already knows everything here is a Nucleus job.
  const short = job.label.replace(/^dev\.nucleus\./, "");

  return (
    <div className="flex items-center gap-3 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-2.5">
      <Icon
        size={14}
        strokeWidth={1.75}
        className={
          kind === "ok"   ? "text-[var(--color-status-ok)]"
        : kind === "warn" ? "text-[var(--color-status-warn)]"
        : kind === "down" ? "text-[var(--color-status-down)]"
        :                   "text-[var(--color-nucleus-faint)]"
        }
      />
      <div className="min-w-0 flex-1">
        <div className="truncate text-sm text-[var(--color-nucleus-text)]" title={job.label}>
          {short}
        </div>
        <div className="mt-0.5 text-[11px] text-[var(--color-nucleus-faint)]">
          {job.pid !== null ? `pid ${job.pid}` : "—"} · exit {job.last_exit ?? "—"}
        </div>
      </div>
      <StatusPill kind={kind}>{label}</StatusPill>
    </div>
  );
}
