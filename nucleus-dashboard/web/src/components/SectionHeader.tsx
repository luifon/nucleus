// Section header — the `┌── label ──` terminal flourish. Used at the
// top of every page section for consistent visual rhythm.
export default function SectionHeader({ label, hint }: { label: string; hint?: string }) {
  // The `──` rule is rendered as a flex-grow span with a top-border
  // so it scales to whatever width the parent gives. Avoids the
  // hard-coded dash-count that breaks when fonts/sizes shift.
  return (
    <div className="mb-3 flex items-end gap-2 text-xs uppercase tracking-widest text-[var(--color-nucleus-faint)] opacity-70">
      <span>┌──</span>
      <span>{label}</span>
      <span className="flex-1 translate-y-[-3px] border-t border-[var(--color-nucleus-border)]" />
      {hint && <span className="text-[10px] normal-case tracking-normal">{hint}</span>}
    </div>
  );
}
