import { X } from "lucide-react";
import type { ReminderRef } from "@/lib/api";
import { reminderLine } from "@/lib/skills";

// The result of the last write on the Skills page (a skill move, archive,
// restore or delete, or a suggestion decision), shown until dismissed or
// replaced by the next write. An error can list the reminders that block
// the action and the problems of a refused draft.

export type Outcome =
  | { kind: "ok" | "warn"; lines: string[] }
  | { kind: "error"; lines: string[]; reminders: ReminderRef[]; issues?: string[] };

const OUTCOME_TONE: Record<Outcome["kind"], { border: string; text: string }> = {
  ok: { border: "border-[var(--color-status-ok)]", text: "text-[var(--color-status-ok)]" },
  warn: { border: "border-[var(--color-status-warn)]", text: "text-[var(--color-status-warn)]" },
  error: { border: "border-[var(--color-status-down)]", text: "text-[var(--color-status-down)]" },
};

export default function OutcomePanel({ outcome, onDismiss }: { outcome: Outcome; onDismiss: () => void }) {
  const tone = OUTCOME_TONE[outcome.kind];
  const [head, ...rest] = outcome.lines;
  return (
    <div
      className={`mb-4 flex items-start gap-3 rounded border bg-[var(--color-nucleus-surface)] px-3 py-2 text-xs ${tone.border}`}
    >
      <div className="min-w-0 flex-1 space-y-1">
        <div className={tone.text}>{head}</div>
        {rest.map((l, i) => (
          <div key={i} className="text-[var(--color-nucleus-faint)]">
            {l}
          </div>
        ))}
        {outcome.kind === "error" && outcome.issues && outcome.issues.length > 0 && (
          <ul className="list-disc space-y-0.5 pl-4 pt-1 text-[var(--color-nucleus-text)]">
            {outcome.issues.map((i, k) => (
              <li key={k}>{i}</li>
            ))}
          </ul>
        )}
        {outcome.kind === "error" && outcome.reminders.length > 0 && (
          <div className="pt-1">
            <div className="text-[var(--color-nucleus-faint)]">
              These reminders refer to the skill. Change or cancel them first:
            </div>
            <ul className="mt-1 space-y-0.5 text-[var(--color-nucleus-text)]">
              {outcome.reminders.map((r) => (
                <li key={`${r.id}-${r.field}`}>
                  <code>{reminderLine(r)}</code>
                </li>
              ))}
            </ul>
          </div>
        )}
      </div>
      <button
        onClick={onDismiss}
        title="dismiss"
        className="shrink-0 text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
      >
        <X size={12} strokeWidth={2} />
      </button>
    </div>
  );
}
