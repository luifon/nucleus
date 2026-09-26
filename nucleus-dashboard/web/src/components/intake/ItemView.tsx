import { useState } from "react";
import { Link } from "react-router-dom";
import { ChevronLeft } from "lucide-react";
import Tabs from "@/components/Tabs";
import { useFetch, usePollWhile } from "@/lib/hooks";
import { getIntakeDetail, type IntakeDetail, type IntakeItem } from "@/lib/api";
import { isOpenItem, planVersions, threadOrder } from "@/lib/intake";
import ItemDetails from "./ItemDetails";
import ItemHeader from "./ItemHeader";
import ItemThread from "./ItemThread";
import PlanPanel from "./PlanPanel";

// The item page (`/intake?item=<n>`, ADR-036): where the operator reads
// the plan, discusses it with the refinement agent and decides. The
// detail is refetched every DETAIL_POLL_MS while the item is not finished.
//
// Layout: below the xl breakpoint one pane at a time (conversation, plan,
// details) under a tab strip, so a phone gets the full width for each; at
// xl and wider the conversation stays on the left and the plan or the
// details on the right.

export const DETAIL_POLL_MS = 10_000;

type Pane = "thread" | "plan" | "details";
type Side = "plan" | "details";

export default function ItemView({ itemId, onChange }: { itemId: number; onChange: (item: IntakeItem) => void }) {
  const detail = useFetch((signal) => getIntakeDetail(itemId, signal), [itemId]);
  const item = detail.data?.item;
  usePollWhile(detail.refetch, !!item && isOpenItem(item.stage), DETAIL_POLL_MS);

  if (!detail.data || detail.data.item.id !== itemId) {
    return (
      <div className="flex h-full flex-col" data-item-id={itemId}>
        <div className="flex items-center gap-2 border-b border-[var(--color-nucleus-border)] px-4 py-3 text-sm md:px-5">
          <Link to="/intake" aria-label="back to items" className="text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)] lg:hidden">
            <ChevronLeft size={18} strokeWidth={1.75} />
          </Link>
          <code className="text-[var(--color-nucleus-faint)]">item #{itemId}</code>
        </div>
        <div className={`px-4 py-4 text-xs md:px-5 ${detail.error ? "text-[var(--color-status-down)]" : "text-[var(--color-nucleus-faint)]"}`}>
          {detail.error ?? "fetching…"}
        </div>
      </div>
    );
  }

  return (
    <ItemScreen
      key={itemId}
      detail={detail.data}
      refreshError={detail.error}
      onChange={(i) => {
        onChange(i);
        detail.refetch();
      }}
    />
  );
}

/** The item page for a fetched detail. `onChange` receives the item an
 *  action returned; the caller refetches. */
export function ItemScreen({
  detail,
  refreshError = null,
  onChange,
  defaultPane,
}: {
  detail: IntakeDetail;
  refreshError?: string | null;
  onChange: (item: IntakeItem) => void;
  defaultPane?: Pane;
}) {
  const { item } = detail;
  const versions = planVersions(detail);
  const messages = threadOrder(detail.messages);
  const initialSide: Side = item.stage === "held" || versions.length === 0 ? "details" : "plan";
  const [pane, setPane] = useState<Pane>(defaultPane ?? (item.stage === "held" ? "details" : "thread"));
  const [side, setSide] = useState<Side>(defaultPane === "plan" || defaultPane === "details" ? defaultPane : initialSide);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const now = Date.now();

  const act = async (fn: () => Promise<IntakeItem>): Promise<boolean> => {
    setBusy(true);
    setErr(null);
    try {
      onChange(await fn());
      return true;
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
      return false;
    } finally {
      setBusy(false);
    }
  };

  const choose = (p: Pane) => {
    setPane(p);
    if (p !== "thread") setSide(p);
  };
  const latest = versions.length > 0 ? versions[versions.length - 1].version : null;
  const planLabel = latest !== null ? `plan v${latest}` : "plan";

  const sideContent =
    side === "plan" ? (
      <PlanPanel item={item} versions={versions} busy={busy} act={act} />
    ) : (
      <div className="min-h-0 flex-1 overflow-y-auto">
        <ItemDetails detail={detail} now={now} busy={busy} act={act} />
      </div>
    );

  return (
    <div className="flex h-full min-h-0 flex-col" data-item-id={item.id}>
      <ItemHeader item={item} event={detail.event} busy={busy} act={act} />
      {(err || refreshError) && (
        <div role="alert" className="shrink-0 border-b border-[var(--color-status-down)] bg-[color-mix(in_srgb,var(--color-status-down)_15%,var(--color-nucleus-surface))] px-4 py-2 text-xs text-[var(--color-status-down)] md:px-5">
          {err ?? `refresh failed: ${refreshError}`}
        </div>
      )}

      <div className="shrink-0 px-2 xl:hidden">
        <Tabs
          className="mb-0"
          value={pane}
          onChange={choose}
          tabs={[
            { value: "thread", label: "conversation", count: messages.length },
            { value: "plan", label: planLabel },
            { value: "details", label: item.stage === "held" ? "details · held" : "details" },
          ]}
        />
      </div>

      <div className="flex min-h-0 flex-1 xl:grid xl:grid-cols-2">
        <section aria-label="conversation" className={`${pane === "thread" ? "flex" : "hidden"} min-h-0 min-w-0 flex-1 flex-col xl:flex xl:border-r xl:border-[var(--color-nucleus-border)]`}>
          <ItemThread item={item} messages={messages} visible={pane === "thread"} onSent={onChange} question={detail.question} findings={detail.hidden} />
        </section>
        <section aria-label={side} className={`${pane === "thread" ? "hidden" : "flex"} min-h-0 min-w-0 flex-1 flex-col xl:flex`}>
          <div className="hidden shrink-0 px-2 xl:block">
            <Tabs
              className="mb-0"
              value={side}
              onChange={(s) => choose(s)}
              tabs={[
                { value: "plan", label: planLabel },
                { value: "details", label: item.stage === "held" ? "details · held" : "details" },
              ]}
            />
          </div>
          {sideContent}
        </section>
      </div>
    </div>
  );
}
