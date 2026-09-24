import { useState, type ReactNode } from "react";
import { Copy, Check } from "lucide-react";
import SectionHeader from "@/components/SectionHeader";
import type { UsageTotals } from "@/lib/api/usage";
import { formatMetric, metricOf, scopeLabel, unpricedNote, type Metric, type VendorChoice } from "@/lib/usage";

// Small building blocks of the usage surface (ADR-034).

export const faint = "text-[var(--color-nucleus-faint)]";

export function Segmented({
  value,
  options,
  onChange,
  label,
}: {
  value: string;
  options: { value: string; label: string }[];
  onChange: (v: string) => void;
  label: string;
}) {
  return (
    <div className="flex items-center gap-2">
      <span className={faint}>{label}</span>
      <div role="radiogroup" aria-label={label} className="flex overflow-hidden rounded border border-[var(--color-nucleus-border)]">
        {options.map((o) => (
          <button
            key={o.value}
            role="radio"
            aria-checked={o.value === value}
            onClick={() => onChange(o.value)}
            className={[
              "px-2.5 py-1 transition-colors",
              o.value === value
                ? "bg-[color-mix(in_srgb,var(--color-nucleus-accent)_14%,transparent)] text-[var(--color-nucleus-accent)]"
                : "text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-text)]",
            ].join(" ")}
          >
            {o.label}
          </button>
        ))}
      </div>
    </div>
  );
}

/** Which tools a figure covers: `[Claude + Codex]`, `[Claude]`, `[Codex]`. */
export function Scope({ vendor }: { vendor: VendorChoice }) {
  return <span className={`text-[10px] ${faint}`}>[{scopeLabel(vendor)}]</span>;
}

/** A metric value; for dollars, followed by the unpriced-token note when
 *  some tokens in the total have no price. */
export function MetricValue({
  metric,
  totals,
  className = "",
}: {
  metric: Metric;
  totals: Pick<UsageTotals, "cost_usd" | "tokens" | "unpriced_tokens">;
  className?: string;
}) {
  const note = metric === "cost" ? unpricedNote(totals) : null;
  return (
    <span className={className}>
      {formatMetric(metric, metricOf(metric, totals))}
      {note && <UnpricedNote text={note} />}
    </span>
  );
}

export function UnpricedNote({ text }: { text: string }) {
  return (
    <span className="ml-2 whitespace-nowrap align-middle text-[10px] text-[var(--color-status-warn)]" title="models missing from the price table; see models › price table">
      [{text}]
    </span>
  );
}

export function Panel({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <section className="mb-8">
      <SectionHeader label={label} hint={hint} />
      {children}
    </section>
  );
}

export function NotApplicable({ children }: { children: ReactNode }) {
  return <div className={`rounded border border-dashed border-[var(--color-nucleus-border)] p-4 text-xs ${faint}`}>{children}</div>;
}

export function Loading<T>({
  state,
  children,
}: {
  state: { data: T | null; error: string | null; loading: boolean };
  children: (d: T) => ReactNode;
}) {
  if (state.error) return <div className="text-sm text-[var(--color-status-down)]">{state.error}</div>;
  if (!state.data) return <div className={`text-sm ${faint}`}>loading…</div>;
  return <div className={state.loading ? "opacity-60" : ""}>{children(state.data)}</div>;
}

export function Table({ head, rows, align }: { head: ReactNode[]; rows: ReactNode[][]; align?: ("l" | "r")[] }) {
  return (
    <div className="overflow-x-auto">
      <table className="w-full text-xs">
        <thead>
          <tr className={`border-b border-[var(--color-nucleus-border)] ${faint}`}>
            {head.map((h, i) => (
              <th key={i} className={`px-2 py-1.5 font-normal ${align?.[i] === "r" ? "text-right" : "text-left"}`}>
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.map((r, ri) => (
            <tr key={ri} className="border-b border-[var(--color-nucleus-border)]/50 hover:bg-[color-mix(in_srgb,var(--color-nucleus-text)_3%,transparent)]">
              {r.map((c, ci) => (
                <td key={ci} className={`px-2 py-1.5 align-top ${align?.[ci] === "r" ? "text-right" : "text-left"}`}>
                  {c}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
      {rows.length === 0 && <div className={`px-2 py-3 text-xs ${faint}`}>nothing in this range</div>}
    </div>
  );
}

export function CopyText({ text, label }: { text: string; label?: string }) {
  const [done, setDone] = useState(false);
  return (
    <button
      title={text}
      onClick={() => {
        void navigator.clipboard.writeText(text).then(() => {
          setDone(true);
          setTimeout(() => setDone(false), 1200);
        });
      }}
      className={`inline-flex max-w-full items-center gap-1 ${faint} hover:text-[var(--color-nucleus-accent)]`}
    >
      {done ? <Check size={11} /> : <Copy size={11} />}
      <span className="truncate">{label ?? text}</span>
    </button>
  );
}
