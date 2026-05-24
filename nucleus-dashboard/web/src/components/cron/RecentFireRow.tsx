import { Check, X, Sparkles, Hash } from "lucide-react";
import { type RecentFire } from "@/lib/api";

// Single row in the recent-fires history list. Compact: when, which
// channel, success/fail dot, title (or body or derived), error tooltip
// on failures.

export default function RecentFireRow({ fire }: { fire: RecentFire }) {
  const ok = fire.success === 1;
  const isSkill = fire.is_skill_fire === 1;
  const display = pickDisplayName(fire);
  return (
    <li
      className="flex items-center gap-2 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-1.5 text-[12px]"
      title={fire.error ?? undefined}
    >
      {ok ? (
        <Check size={11} strokeWidth={2} className="shrink-0 text-[var(--color-status-ok)]" />
      ) : (
        <X size={11} strokeWidth={2} className="shrink-0 text-[var(--color-status-down)]" />
      )}
      <span className="shrink-0 text-[var(--color-nucleus-faint)] tabular-nums">
        {firedTime(fire.fired_at)}
      </span>
      <span className="shrink-0 rounded border border-[var(--color-nucleus-border)] px-1.5 py-px text-[10px] text-[var(--color-nucleus-faint)]">
        {fire.channel}
      </span>
      {isSkill && (
        <Sparkles
          size={10}
          strokeWidth={1.75}
          className="shrink-0 text-[var(--color-nucleus-accent)]"
          aria-label="skill-fire"
        />
      )}
      <span
        className={`min-w-0 flex-1 truncate ${display.derived ? "italic text-[var(--color-nucleus-faint)]" : "text-[var(--color-nucleus-text)]"}`}
      >
        {display.text}
      </span>
      <span className="shrink-0 text-[10px] text-[var(--color-nucleus-faint)]">
        <Hash size={9} strokeWidth={2} className="inline align-middle" />
        {fire.reminder_id}
      </span>
    </li>
  );
}

// Same priority order as UpcomingFireRow: title > body > derived.
function pickDisplayName(fire: RecentFire): { text: string; derived: boolean } {
  if (fire.reminder_title && fire.reminder_title.trim()) {
    return { text: fire.reminder_title, derived: false };
  }
  if (fire.reminder_body && fire.reminder_body.trim()) {
    return { text: fire.reminder_body, derived: false };
  }
  return { text: "[skill-fire]", derived: true };
}

function firedTime(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  const now = new Date();
  const sameDay =
    d.getFullYear() === now.getFullYear() &&
    d.getMonth() === now.getMonth() &&
    d.getDate() === now.getDate();
  const time = d.toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });
  if (sameDay) return time;
  const day = d.toLocaleDateString("en-GB", { day: "2-digit", month: "2-digit" });
  return `${day} ${time}`;
}
