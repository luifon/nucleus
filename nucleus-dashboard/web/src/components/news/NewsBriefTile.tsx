import { type NewsBrief } from "@/lib/api";

// The day's brief, as the widget shows it above the list (ADR-031). A brief
// the widget would no longer show stays readable here, faded, with the reason.
export default function NewsBriefTile({ brief }: { brief: NewsBrief }) {
  if (!brief.text.trim()) return null;
  const withdrawn =
    brief.standing === "names_downvoted"
      ? "The widget would not show this brief again: it was written from an item you downvoted since."
      : brief.standing === "unverifiable"
        ? "This brief predates the record of its items, so it cannot be checked against your downvotes."
        : null;
  const written = new Date(brief.created_at);
  const time = Number.isNaN(written.getTime())
    ? brief.created_at
    : written.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  return (
    <section className="mb-6 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-4 py-3">
      <div className="mb-1.5 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)]">
        brief · {time}
      </div>
      <p
        className={`text-sm leading-relaxed ${
          withdrawn ? "text-[var(--color-nucleus-faint)]" : "text-[var(--color-nucleus-text)]"
        }`}
      >
        {brief.text}
      </p>
      {withdrawn && <p className="mt-2 text-[11px] text-[var(--color-status-warn)]">{withdrawn}</p>}
    </section>
  );
}
