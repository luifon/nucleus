import { useEffect, useMemo, useRef, useState } from "react";
import { Check } from "lucide-react";
import InlineConfirm from "@/components/InlineConfirm";
import Markdown from "@/components/Markdown";
import Select from "@/components/Select";
import { approvePlan, type WorkItem, type WorkPlanVersion } from "@/lib/api";
import { canApproveShown, planStatus } from "@/lib/work";
import DiffView, { DiffCount } from "@/components/DiffView";
import ViewTab from "@/components/ViewTab";
import { lineDiff } from "@/lib/linediff";
import { shortTime } from "@/lib/tasks";
import { ActionButton } from "./parts";

// The plan: the selected version rendered as Markdown (the latest by
// default), its approval state, a selector over every version, and the
// line changes from the version before it. "Approve plan vN" sits here,
// bound to the version on screen, and shows only while that version is
// the proposal the item waits on.

type View = "plan" | "changes";

/** A request to show plan `version`; a new `nonce` repeats it. */
export interface PlanFocus {
  version: number;
  nonce: number;
}

export default function PlanPanel({
  item,
  versions,
  busy,
  act,
  defaultVersion,
  defaultView = "plan",
  focus = null,
}: {
  item: WorkItem;
  versions: readonly WorkPlanVersion[];
  busy: boolean;
  act: (fn: () => Promise<WorkItem>) => Promise<boolean>;
  /** The version shown first; the latest when absent. */
  defaultVersion?: number;
  defaultView?: View;
  /** A version the conversation asked to show (its "view" link). */
  focus?: PlanFocus | null;
}) {
  const latest = versions.length > 0 ? versions[versions.length - 1].version : null;
  const [picked, setPicked] = useState<number | null>(defaultVersion ?? null);
  const [view, setView] = useState<View>(defaultView);
  const [confirm, setConfirm] = useState(false);

  // Follow the newest version while the operator has not picked an older
  // one: a new plan arriving by the 10 s refresh replaces the one shown.
  const followLatest = useRef(defaultVersion === undefined);
  useEffect(() => {
    if (followLatest.current) setPicked(latest);
  }, [latest]);

  // "view" on a plan line in the conversation: show that version.
  useEffect(() => {
    if (!focus) return;
    followLatest.current = focus.version === latest;
    setPicked(focus.version);
    setView("plan");
    setConfirm(false);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [focus?.nonce]);

  const idx = versions.findIndex((v) => v.version === (picked ?? latest));
  const shown = idx >= 0 ? versions[idx] : versions[versions.length - 1];
  const previous = idx > 0 ? versions[idx - 1] : null;
  const shownVersion = shown?.version ?? null;

  const diff = useMemo(() => (previous && shown ? lineDiff(previous.text, shown.text) : null), [previous, shown]);

  if (!shown) {
    return <div className="px-4 py-4 text-xs text-[var(--color-nucleus-faint)] md:px-5">no plan yet</div>;
  }

  const status = planStatus(shown.version, item);
  const approvable = canApproveShown(item, shownVersion);

  return (
    <div className="flex min-h-0 flex-1 flex-col" data-plan-version={shown.version}>
      <div className="shrink-0 space-y-2 border-b border-[var(--color-nucleus-border)] px-4 py-2.5 md:px-5">
        <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
          <Select
            label="plan"
            value={String(shown.version)}
            onChange={(v) => {
              const n = Number(v);
              followLatest.current = n === latest;
              setPicked(n);
              setConfirm(false);
            }}
            options={[...versions].reverse().map((v) => ({
              value: String(v.version),
              label: `v${v.version} · ${planStatus(v.version, item)} · ${shortTime(v.at)}`,
            }))}
          />
          <PlanStatus status={status} item={item} />
          {approvable && !confirm && (
            <span className="ml-auto">
              <ActionButton onClick={() => setConfirm(true)} disabled={busy}>
                <Check size={11} strokeWidth={2} />
                approve plan v{shown.version}
              </ActionButton>
            </span>
          )}
        </div>
        <div role="tablist" className="flex items-center gap-1 text-xs">
          <ViewTab active={view === "plan"} onClick={() => setView("plan")}>
            plan
          </ViewTab>
          <ViewTab active={view === "changes"} onClick={() => setView("changes")} disabled={!previous}>
            {previous ? `changes from v${previous.version}` : "no earlier version"}
          </ViewTab>
          {diff && view === "changes" && <DiffCount rows={diff} />}
        </div>
      </div>
      {approvable && confirm && (
        <InlineConfirm
          className="shrink-0 px-4 py-2 md:px-5"
          message={`Approve plan v${shown.version}? The implementation agent starts from it. Refused if a newer version arrived.`}
          confirmLabel={busy ? "approving…" : `approve v${shown.version}`}
          busy={busy}
          onConfirm={() => void act(() => approvePlan(item.id, shown.version)).then((ok) => ok && setConfirm(false))}
          onCancel={() => setConfirm(false)}
        />
      )}
      <div className="min-h-0 flex-1 overflow-y-auto px-4 py-4 md:px-5">
        {view === "changes" && diff ? <DiffView ops={diff} /> : <Markdown source={shown.text} />}
      </div>
    </div>
  );
}

function PlanStatus({ status, item }: { status: ReturnType<typeof planStatus>; item: WorkItem }) {
  if (status === "approved") {
    return (
      <span className="text-xs text-[var(--color-status-ok)]">
        approved{item.approved_via ? ` via ${item.approved_via}` : ""}
        {item.approved_at ? ` ${shortTime(item.approved_at)}` : ""}
      </span>
    );
  }
  if (status === "proposed") {
    return <span className="text-xs text-[var(--color-nucleus-accent)]">proposed, not approved</span>;
  }
  return <span className="text-xs text-[var(--color-nucleus-faint)]">replaced by a later version</span>;
}
