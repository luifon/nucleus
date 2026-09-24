import { useState } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";
import { useFetch } from "@/lib/hooks";
import { type CheckReport, type CheckRunSummary, getLatestVaultCheck, listVaultCheckRuns } from "@/lib/api";
import { COUNT_COLUMNS, countDelta, formatDelta, groupFindings, type FindingGroup } from "@/lib/vault";

// ADR-035 weekly vault check: the latest report (counts with the change
// since the previous run, findings grouped by kind) and the run history.
// Written by `nucleus vault-check`; this page only reads it.

export default function VaultCheckPanel({ refreshKey }: { refreshKey: number }) {
  const latest = useFetch((signal) => getLatestVaultCheck(signal), [refreshKey]);
  const runs = useFetch((signal) => listVaultCheckRuns(26, signal), [refreshKey]);

  if (latest.error || runs.error) {
    return (
      <div className="rounded border border-[var(--color-status-down)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-sm text-[var(--color-status-down)]">
        {latest.error ?? runs.error}
      </div>
    );
  }
  if (latest.loading || runs.loading) {
    return <div className="text-sm text-[var(--color-nucleus-faint)]">fetching…</div>;
  }
  if (!latest.data) {
    return (
      <div className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-6 text-sm text-[var(--color-nucleus-faint)]">
        No vault check has run yet. Run <code>nucleus vault-check</code>, or wait for the weekly run.
      </div>
    );
  }
  return (
    <div className="space-y-6">
      <ReportHeader report={latest.data} />
      <CountTiles report={latest.data} runs={runs.data ?? []} />
      <Findings groups={groupFindings(latest.data.findings)} />
      <Trend runs={runs.data ?? []} />
    </div>
  );
}

function ReportHeader({ report }: { report: CheckReport }) {
  return (
    <div className="text-xs text-[var(--color-nucleus-faint)]">
      run #{report.run_id} · {new Date(report.started_at).toLocaleString("en-GB")} · {report.trigger} ·{" "}
      {report.applied ? "fixes applied" : "report only"} · {report.notes_scanned} notes · {report.files_excluded} excluded ·{" "}
      {report.duration_ms} ms
    </div>
  );
}

function CountTiles({ report, runs }: { report: CheckReport; runs: CheckRunSummary[] }) {
  // Deltas only make sense when the latest run is the newest summary.
  const aligned = runs.length > 0 && runs[0].id === report.run_id;
  return (
    <div className="grid grid-cols-2 gap-2 sm:grid-cols-4 lg:grid-cols-9">
      {COUNT_COLUMNS.map(({ key, label }) => {
        const value = report.counts[key];
        const delta = aligned ? countDelta(runs, key) : null;
        return (
          <div
            key={key}
            className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-2"
          >
            <div className="text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)]">{label}</div>
            <div className="mt-1 flex items-baseline gap-2">
              <span className={`text-lg tabular-nums ${value > 0 && key !== "fixed" ? "text-[var(--color-nucleus-accent)]" : ""}`}>
                {value}
              </span>
              {delta !== null && (
                <span className="text-[11px] tabular-nums text-[var(--color-nucleus-faint)]">{formatDelta(delta)}</span>
              )}
            </div>
          </div>
        );
      })}
    </div>
  );
}

function Findings({ groups }: { groups: FindingGroup[] }) {
  if (groups.length === 0) {
    return <div className="text-sm text-[var(--color-status-ok)]">no findings</div>;
  }
  return (
    <div className="space-y-1.5">
      {groups.map((g) => (
        <FindingSection key={g.kind} group={g} />
      ))}
    </div>
  );
}

function FindingSection({ group }: { group: FindingGroup }) {
  const [open, setOpen] = useState(false);
  return (
    <section className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)]">
      <button
        onClick={() => setOpen(!open)}
        className="flex w-full items-center gap-2 px-4 py-2.5 text-left text-sm hover:bg-[var(--color-nucleus-bg)]"
      >
        {open ? (
          <ChevronDown size={14} strokeWidth={1.75} className="text-[var(--color-nucleus-faint)]" />
        ) : (
          <ChevronRight size={14} strokeWidth={1.75} className="text-[var(--color-nucleus-faint)]" />
        )}
        <span>{group.label}</span>
        <span className="text-xs tabular-nums text-[var(--color-nucleus-faint)]">({group.findings.length})</span>
      </button>
      {open && (
        <ul className="space-y-2 border-t border-[var(--color-nucleus-border)] px-4 py-3 text-xs">
          {group.findings.map((f, i) => (
            <li key={i}>
              <div className="flex flex-wrap items-baseline gap-2">
                {f.path && <span className="text-[var(--color-nucleus-text)]">{f.path}</span>}
                <span className="text-[var(--color-nucleus-faint)]">{f.detail}</span>
                {f.fix_action && (
                  <span className={f.fixed ? "text-[var(--color-status-ok)]" : "text-[var(--color-status-warn)]"}>
                    [{f.fix_action}]
                  </span>
                )}
              </div>
              {f.related.length > 0 && (
                <ul className="mt-1 space-y-0.5 pl-4 text-[var(--color-nucleus-faint)]">
                  {f.related.map((r) => (
                    <li key={r}>{r}</li>
                  ))}
                </ul>
              )}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

function Trend({ runs }: { runs: CheckRunSummary[] }) {
  if (runs.length === 0) return null;
  return (
    <div>
      <div className="mb-1.5 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)] opacity-70">
        history (newest first)
      </div>
      <div className="overflow-x-auto rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)]">
        <table className="w-full text-xs tabular-nums">
          <thead className="text-[var(--color-nucleus-faint)]">
            <tr>
              <th className="px-3 py-2 text-left font-normal">run</th>
              <th className="px-3 py-2 text-left font-normal">date</th>
              <th className="px-3 py-2 text-left font-normal">trigger</th>
              {COUNT_COLUMNS.map((c) => (
                <th key={c.key} className="px-3 py-2 text-right font-normal">
                  {c.label}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {runs.map((r) => (
              <tr key={r.id} className="border-t border-[var(--color-nucleus-border)]">
                <td className="px-3 py-1.5">#{r.id}</td>
                <td className="px-3 py-1.5">{r.started_at.slice(0, 10)}</td>
                <td className="px-3 py-1.5 text-[var(--color-nucleus-faint)]">
                  {r.trigger}
                  {r.applied ? " · fixes" : ""}
                </td>
                {COUNT_COLUMNS.map((c) => (
                  <td key={c.key} className="px-3 py-1.5 text-right">
                    {r.counts[c.key]}
                  </td>
                ))}
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </div>
  );
}
