import { useState } from "react";
import { collapseUnchanged, diffStats, type DiffOp, type DiffRow } from "@/lib/linediff";

// Line changes between two texts (lib/linediff): the work plan panel shows
// the changes from the previous plan version, the Skills page the changes a
// patch suggestion makes to the live SKILL.md.

export function DiffCount({ rows }: { rows: readonly DiffOp[] }) {
  const { added, removed } = diffStats(rows);
  return (
    <span className="ml-auto text-[11px]">
      <span className="text-[var(--color-status-ok)]">+{added}</span> <span className="text-[var(--color-status-down)]">−{removed}</span>
    </span>
  );
}

/** The line changes, with unchanged runs collapsed to three lines of
 *  context; a collapsed run expands when clicked. */
export default function DiffView({ ops }: { ops: readonly DiffOp[] }) {
  const [expanded, setExpanded] = useState(false);
  const rows: readonly DiffRow[] = expanded ? ops : collapseUnchanged(ops, 3);
  if (ops.every((o) => o.kind === "same")) {
    return <div className="text-xs text-[var(--color-nucleus-faint)]">no line changed</div>;
  }
  return (
    <pre className="overflow-x-auto rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] py-1 text-xs leading-relaxed">
      {rows.map((r, k) => {
        if (r.kind === "skip") {
          return (
            <button
              key={k}
              onClick={() => setExpanded(true)}
              className="block w-full px-3 py-0.5 text-left text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
            >
              ⋯ {r.count} unchanged {r.count === 1 ? "line" : "lines"}
            </button>
          );
        }
        const cls =
          r.kind === "add"
            ? "bg-[color-mix(in_srgb,var(--color-status-ok)_18%,transparent)] text-[var(--color-nucleus-text)]"
            : r.kind === "del"
              ? "bg-[color-mix(in_srgb,var(--color-status-down)_18%,transparent)] text-[var(--color-nucleus-faint)]"
              : "text-[var(--color-nucleus-faint)]";
        const sign = r.kind === "add" ? "+" : r.kind === "del" ? "−" : " ";
        return (
          <div key={k} data-diff={r.kind} className={`whitespace-pre-wrap break-words px-3 ${cls}`}>
            <span className="mr-2 select-none opacity-70">{sign}</span>
            {r.text || " "}
          </div>
        );
      })}
    </pre>
  );
}
