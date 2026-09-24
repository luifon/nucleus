import { useEffect, useRef, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { RefreshCw } from "lucide-react";
import PageShell from "@/components/PageShell";
import Tabs from "@/components/Tabs";
import {
  BarList,
  COLOR,
  Heatmap,
  Legend,
  PercentLine,
  StackedColumns,
  Timeline,
  type BarRow,
} from "@/components/usage/charts";
import {
  CopyText,
  Loading,
  MetricValue,
  NotApplicable,
  Panel,
  Scope,
  Segmented,
  Table,
  UnpricedNote,
  faint,
} from "@/components/usage/ui";
import { useFetch } from "@/lib/hooks";
import {
  getUsageLimits,
  getUsageNucleus,
  getUsageProjects,
  getUsageSessions,
  getUsageStatus,
  getUsageSummary,
  startUsageRefresh,
  type UsageCompare,
  type UsageStatus,
  type UsageSummary,
} from "@/lib/api/usage";
import {
  addDays,
  cacheHitRatio,
  deltaPct,
  formatMetric,
  formatTokens,
  formatUsd,
  heatGrid,
  metricOf,
  parseVendor,
  pivotSeries,
  scopeLabel,
  unpricedNote,
  weekStart,
  type Metric,
  type VendorChoice,
} from "@/lib/usage";

// ADR-034 — usage accounting. Every Claude Code and Codex session on the
// machine, per project, model, Nucleus agent and reminder. Dollar figures
// are estimates at API list price; nothing here is an invoice.
//
// Page state (tab, range, metric, tool filter) lives in the URL so a reload
// or a shared link shows the same view. The tool filter is sent to the API
// and applied in SQL; every headline figure states the tools it covers.

type TabValue = "overview" | "projects" | "nucleus" | "models" | "limits" | "sessions";
const TABS: TabValue[] = ["overview", "projects", "nucleus", "models", "limits", "sessions"];
const RANGES = [
  { days: 7, label: "7d" },
  { days: 30, label: "30d" },
  { days: 90, label: "90d" },
  { days: 0, label: "all" },
];
/** Start a refresh on open when the data is older than this. */
const STALE_MS = 30 * 60 * 1000;
const POLL_MS = 3000;

/** Everything a view needs to fetch and label its data. */
type View = { days: number; metric: Metric; vendor: VendorChoice; version: number };

function useUrlState() {
  const [params, setParams] = useSearchParams();
  const tab = (TABS as string[]).includes(params.get("tab") ?? "") ? (params.get("tab") as TabValue) : "overview";
  const daysRaw = Number(params.get("days") ?? "30");
  const days = RANGES.some((r) => r.days === daysRaw) ? daysRaw : 30;
  const metric: Metric = params.get("metric") === "tokens" ? "tokens" : "cost";
  const vendor = parseVendor(params.get("tool"));
  const set = (key: string, value: string, fallback: string) =>
    setParams(
      (p) => {
        const next = new URLSearchParams(p);
        if (value === fallback) next.delete(key);
        else next.set(key, value);
        return next;
      },
      { replace: true },
    );
  return { tab, days, metric, vendor, set };
}

export default function UsagePage() {
  const url = useUrlState();
  const [version, setVersion] = useState(0);
  const refresh = useRefresh(() => setVersion((v) => v + 1));
  const view: View = { days: url.days, metric: url.metric, vendor: url.vendor, version };

  return (
    <PageShell
      title={
        <>
          usage <span className={faint}>/ claude code + codex</span>
        </>
      }
      subtitle="Tokens per project, model, agent and reminder. Dollar figures are estimates at API list price; the subscriptions are not billed per token."
      actions={<RefreshControl state={refresh} />}
    >
      <div className="mb-5 flex flex-wrap items-center gap-x-6 gap-y-3 text-xs">
        <Segmented
          label="tool"
          value={url.vendor}
          options={[
            { value: "all", label: "all" },
            { value: "claude", label: "claude" },
            { value: "codex", label: "codex" },
          ]}
          onChange={(v) => url.set("tool", v, "all")}
        />
        <Segmented
          label="range"
          value={String(url.days)}
          options={RANGES.map((r) => ({ value: String(r.days), label: r.label }))}
          onChange={(v) => url.set("days", v, "30")}
        />
        <Segmented
          label="measure"
          value={url.metric}
          options={[
            { value: "cost", label: "$ estimate" },
            { value: "tokens", label: "tokens" },
          ]}
          onChange={(v) => url.set("metric", v, "cost")}
        />
      </div>
      <Tabs
        tabs={TABS.map((t) => ({ value: t, label: t }))}
        value={url.tab}
        onChange={(t) => url.set("tab", t, "overview")}
      />
      {refresh.status && !refresh.status.has_data ? (
        <div className={`text-sm ${faint}`}>
          No usage data yet. {refresh.status.refreshing ? "The first refresh is running." : "Start a refresh."}
        </div>
      ) : (
        <>
          {url.tab === "overview" && <Overview view={view} completeSince={refresh.status?.claude_complete_since ?? null} />}
          {url.tab === "projects" && <Projects view={view} />}
          {url.tab === "nucleus" && <Nucleus view={view} />}
          {url.tab === "models" && <Models view={view} status={refresh.status} />}
          {url.tab === "limits" && <Limits view={view} />}
          {url.tab === "sessions" && <Sessions view={view} />}
        </>
      )}
    </PageShell>
  );
}

// ─── refresh ───────────────────────────────────────────────────────────────

type RefreshState = {
  status: UsageStatus | null;
  error: string | null;
  start: () => void;
};

/** Status polling + refresh trigger. Starts a refresh on open when the data
 *  is stale; polls while one runs; bumps the data version when it ends. */
function useRefresh(onDone: () => void): RefreshState {
  const [status, setStatus] = useState<UsageStatus | null>(null);
  const [error, setError] = useState<string | null>(null);
  const autoStarted = useRef(false);
  const wasRunning = useRef(false);
  const onDoneRef = useRef(onDone);
  onDoneRef.current = onDone;

  const load = async () => {
    try {
      const s = await getUsageStatus();
      setStatus(s);
      if (wasRunning.current && !s.refreshing) onDoneRef.current();
      wasRunning.current = s.refreshing;
      return s;
    } catch (e) {
      setError(String(e));
      return null;
    }
  };

  const start = async () => {
    setError(null);
    try {
      await startUsageRefresh();
      wasRunning.current = true;
      setStatus((s) => (s ? { ...s, refreshing: true } : s));
    } catch (e) {
      setError(String(e));
    }
  };

  useEffect(() => {
    void load().then((s) => {
      if (!s || autoStarted.current || s.refreshing) return;
      const last = s.last_refresh?.finished_at ? Date.parse(s.last_refresh.finished_at) : 0;
      if (Date.now() - last > STALE_MS) {
        autoStarted.current = true;
        void start();
      }
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    if (!status?.refreshing) return;
    const t = setInterval(() => void load(), POLL_MS);
    return () => clearInterval(t);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [status?.refreshing]);

  return { status, error, start: () => void start() };
}

function RefreshControl({ state }: { state: RefreshState }) {
  const s = state.status;
  const last = s?.last_refresh?.finished_at;
  return (
    <div className="flex items-center gap-3 text-xs">
      <span className={faint}>
        {!s ? "…" : s.refreshing ? "refreshing…" : last ? `data as of ${new Date(last).toLocaleString()}` : "never refreshed"}
        {s?.last_refresh?.error && <span className="ml-2 text-[var(--color-status-down)]">[REFRESH FAILED]</span>}
        {state.error && <span className="ml-2 text-[var(--color-status-down)]">{state.error}</span>}
      </span>
      <button
        onClick={state.start}
        disabled={s?.refreshing}
        className="flex items-center gap-1.5 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-2.5 py-1 text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)] disabled:opacity-50"
      >
        <RefreshCw size={12} strokeWidth={1.75} className={s?.refreshing ? "animate-spin" : ""} />
        refresh
      </button>
    </div>
  );
}

/** Legend entries for the tools in scope (one tool → no legend needed). */
function vendorLegend(v: VendorChoice) {
  const all = [
    { label: "claude", color: COLOR.claude },
    { label: "codex", color: COLOR.codex },
  ];
  return v === "all" ? all : [];
}

// ─── overview ──────────────────────────────────────────────────────────────

function Kpi({ cmp, view }: { cmp: UsageCompare; view: View }) {
  const cur = metricOf(view.metric, cmp.current);
  const prev = metricOf(view.metric, cmp.previous);
  const d = cmp.previous_from ? deltaPct(cur, prev) : null;
  return (
    <div className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] p-4">
      <div className={`flex items-baseline justify-between gap-2 text-xs ${faint}`}>
        <span>{cmp.label}</span>
        <Scope vendor={view.vendor} />
      </div>
      <div className="mt-1">
        <MetricValue metric={view.metric} totals={cmp.current} className="text-2xl" />
      </div>
      {cmp.previous_from && (
        <div className={`mt-1 text-xs ${faint}`}>
          {d === null ? "no usage in the previous period" : `${d >= 0 ? "▲" : "▼"} ${Math.abs(d).toFixed(0)}%`}
          {d !== null && ` vs ${formatMetric(view.metric, prev)}`}
        </div>
      )}
    </div>
  );
}

function Overview({ view, completeSince }: { view: View; completeSince: string | null }) {
  const s = useFetch((sig) => getUsageSummary(view.days, view.vendor, sig), [view.days, view.vendor, view.version]);
  return <Loading state={s}>{(d) => <OverviewBody d={d} view={view} completeSince={completeSince} />}</Loading>;
}

function OverviewBody({ d, view, completeSince }: { d: UsageSummary; view: View; completeSince: string | null }) {
  const { days, metric, vendor } = view;
  const all = d.range.current;
  const hit = cacheHitRatio(all);
  const from = days === 0 ? (d.daily[0]?.bucket ?? d.today) : addDays(d.today, -(days - 1));
  const daily = pivotSeries(d.daily, metric, from, d.today);
  const weeklyFrom = d.weekly[0]?.bucket ?? weekStart(d.today);
  const weekly = pivotSeries(d.weekly, metric, weeklyFrom, weekStart(d.today), 7);
  const reachesBack =
    vendor !== "codex" && [d.range.previous_from, from, weeklyFrom].some((x) => x && completeSince && x < completeSince);
  const quota = d.codex_quota.find((q) => q.slot === "primary");
  const legend = vendorLegend(vendor);
  return (
    <>
      {reachesBack && (
        <p className={`mb-4 max-w-3xl text-xs leading-relaxed ${faint}`}>
          Claude data before {completeSince} is incomplete: Claude Code deletes transcripts after about 30 days, and the
          first refresh could only read the ones still on disk. Periods reaching before that day understate Claude usage.
          Codex logs are kept, so Codex history is complete.
        </p>
      )}
      <div className="mb-8 grid grid-cols-1 gap-3 sm:grid-cols-2 xl:grid-cols-4">
        <Kpi cmp={d.day} view={view} />
        <Kpi cmp={d.week} view={view} />
        <Kpi cmp={d.range} view={view} />
        <div className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] p-4">
          <div className={`flex items-baseline justify-between gap-2 text-xs ${faint}`}>
            <span>cache reads, share of input-side tokens</span>
            <Scope vendor={vendor} />
          </div>
          <div className="mt-1 text-2xl">{hit === null ? "—" : `${(hit * 100).toFixed(1)}%`}</div>
          <div className={`mt-1 text-xs ${faint}`}>
            {formatTokens(all.cache_read)} read · {formatTokens(all.cache_write)} written · {formatTokens(all.input)} uncached
          </div>
        </div>
      </div>

      <div className="mb-8 grid grid-cols-1 gap-3 md:grid-cols-3">
        {d.by_vendor.map((v) => (
          <div key={v.vendor} className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] p-4 text-xs">
            <div className="mb-1 flex items-center gap-2">
              <span className="inline-block h-2.5 w-2.5 rounded-[2px]" style={{ background: v.vendor === "codex" ? COLOR.codex : COLOR.claude }} />
              <span className="text-sm">{v.vendor === "codex" ? "Codex" : "Claude"} only</span>
              <span className={`ml-auto ${faint}`}>{days === 0 ? "all time" : `last ${days} days`}</span>
            </div>
            <div className="text-lg">
              {formatUsd(v.totals.cost_usd)}
              {unpricedNote(v.totals) && <UnpricedNote text={unpricedNote(v.totals)!} />}
              <span className={faint}> · {formatTokens(v.totals.tokens)} tokens</span>
            </div>
            <div className={faint}>
              {v.sessions} sessions · {formatTokens(v.totals.output)} output · {formatTokens(v.totals.reasoning)} reasoning
            </div>
          </div>
        ))}
        {d.by_vendor.length === 0 && <NotApplicable>No {scopeLabel(vendor)} usage in this range.</NotApplicable>}
        {vendor === "claude" ? (
          <NotApplicable>
            Codex weekly limit: not applicable to the Claude filter. Claude transcripts record no quota reading.
          </NotApplicable>
        ) : (
          <div className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] p-4 text-xs">
            <div className="mb-1 text-sm">Codex weekly limit</div>
            {quota ? (
              <>
                <div className="text-lg">{quota.used_percent.toFixed(0)}% used</div>
                <div className={faint}>
                  last recorded {new Date(quota.read_at).toLocaleString()}
                  {quota.resets_at && ` · resets ${new Date(quota.resets_at).toLocaleString()}`}
                </div>
              </>
            ) : (
              <div className={faint}>no reading in the Codex logs</div>
            )}
            <div className={`mt-1 ${faint}`}>Claude quota is not recorded in transcripts.</div>
          </div>
        )}
      </div>

      <Panel label={`per day · ${scopeLabel(vendor)}`} hint={days === 0 ? "all time" : `last ${days} days`}>
        {legend.length > 0 && (
          <div className="mb-2">
            <Legend items={legend} />
          </div>
        )}
        <StackedColumns rows={daily} metric={metric} />
      </Panel>
      <Panel label={`per week · ${scopeLabel(vendor)}`} hint="weeks start Monday">
        {legend.length > 0 && (
          <div className="mb-2">
            <Legend items={legend} />
          </div>
        )}
        <StackedColumns rows={weekly} metric={metric} bucketLabel={(b) => `week of ${b}`} />
      </Panel>
      <Panel label={`hour of day × weekday · ${scopeLabel(vendor)}`} hint="API responses only · operator local time (NUCLEUS_TZ)">
        <Heatmap grid={heatGrid(d.heatmap, metric)} metric={metric} />
      </Panel>
    </>
  );
}

// ─── projects ──────────────────────────────────────────────────────────────

function Projects({ view }: { view: View }) {
  const { metric, vendor } = view;
  const p = useFetch((sig) => getUsageProjects(view.days, vendor, sig), [view.days, vendor, view.version]);
  return (
    <Loading state={p}>
      {(rows) => {
        const claudeV = (r: (typeof rows)[number]) => (metric === "cost" ? r.claude_cost_usd : r.claude_tokens);
        const codexV = (r: (typeof rows)[number]) => (metric === "cost" ? r.codex_cost_usd : r.codex_tokens);
        const bars: BarRow[] = rows.slice(0, 20).map((r) => ({
          key: r.root ?? r.project,
          label: r.project,
          sub: `${r.sessions} sessions · last ${r.last_day ?? "—"}`,
          segments: [
            { value: claudeV(r), color: COLOR.claude, label: `claude ${formatMetric(metric, claudeV(r))}` },
            { value: codexV(r), color: COLOR.codex, label: `codex ${formatMetric(metric, codexV(r))}` },
          ].filter((s) => vendor === "all" || s.label.startsWith(vendor)),
          value: formatMetric(metric, metricOf(metric, r.totals)) + (metric === "cost" && r.totals.unpriced_tokens > 0 ? " *" : ""),
        }));
        const anyUnpriced = rows.some((r) => r.totals.unpriced_tokens > 0);
        return (
          <>
            <Panel label={`by project · ${scopeLabel(vendor)}`} hint="repository root; worktrees and subdirectories fold into their repo">
              {vendorLegend(vendor).length > 0 && (
                <div className="mb-3">
                  <Legend items={vendorLegend(vendor)} />
                </div>
              )}
              <BarList rows={bars} />
              {metric === "cost" && anyUnpriced && (
                <div className={`mt-2 text-xs ${faint}`}>* includes tokens without a price; see the table.</div>
              )}
            </Panel>
            <Panel label="table">
              <Table
                head={[
                  "project",
                  "root",
                  "sessions",
                  "tokens",
                  "cache read",
                  ...(vendor === "all" ? ["Claude $", "Codex $"] : []),
                  `${scopeLabel(vendor)} $`,
                  "last day",
                ]}
                align={["l", "l", "r", "r", "r", ...(vendor === "all" ? (["r", "r"] as const) : []), "r", "r"]}
                rows={rows.map((r) => [
                  r.project,
                  <span className={faint}>{r.root ?? "—"}</span>,
                  r.sessions,
                  formatTokens(r.totals.tokens),
                  formatTokens(r.totals.cache_read),
                  ...(vendor === "all" ? [formatUsd(r.claude_cost_usd), formatUsd(r.codex_cost_usd)] : []),
                  <MetricValue metric="cost" totals={r.totals} />,
                  r.last_day ?? "—",
                ])}
              />
            </Panel>
          </>
        );
      }}
    </Loading>
  );
}

// ─── nucleus ───────────────────────────────────────────────────────────────

function Nucleus({ view }: { view: View }) {
  const { metric, vendor } = view;
  const n = useFetch((sig) => getUsageNucleus(view.days, vendor, sig), [view.days, vendor, view.version]);
  if (vendor === "codex") {
    return (
      <NotApplicable>
        Not applicable to the Codex filter: Nucleus agents and reminder fires run on Claude Code sessions
        only, so there is no Codex usage to attribute to them.
      </NotApplicable>
    );
  }
  return (
    <Loading state={n}>
      {(v) => {
        const { agents, reminders } = v.usage;
        const scheduled = new Set(v.scheduled_agents);
        const recurring = [
          ...reminders
            .filter((r) => r.cron && ["active", "pending", "paused"].includes(r.status ?? ""))
            .map((r) => ({
              key: `r${r.reminder_id}`,
              name: r.title ?? `reminder #${r.reminder_id}`,
              kind: `reminder #${r.reminder_id}`,
              schedule: r.cron ?? "",
              runs: r.sessions_30d,
              cost: r.cost_30d,
            })),
          ...agents
            .filter((a) => scheduled.has(a.agent))
            .map((a) => ({ key: `a${a.agent}`, name: a.agent, kind: "agent", schedule: "launchd", runs: a.sessions_30d, cost: a.cost_30d })),
        ]
          .filter((r) => r.runs > 0)
          .sort((a, b) => b.cost - a.cost);
        const monthly = recurring.reduce((a, r) => a + r.cost, 0);
        return (
          <>
            <Panel label="recurring jobs" hint="actual cost over the last 30 days">
              <div className="mb-3 text-sm">
                {formatUsd(monthly)} <span className={faint}>per 30 days across {recurring.length} recurring jobs</span>{" "}
                <Scope vendor="claude" />
              </div>
              <Table
                head={["job", "kind", "schedule", "runs (30d)", "per run", "last 30 days"]}
                align={["l", "l", "l", "r", "r", "r"]}
                rows={recurring.map((r) => [
                  r.name,
                  <span className={faint}>{r.kind}</span>,
                  <code className={faint}>{r.schedule}</code>,
                  r.runs,
                  formatUsd(r.runs ? r.cost / r.runs : 0),
                  formatUsd(r.cost),
                ])}
              />
            </Panel>
            <Panel label="by agent" hint={`sessions in ${v.workspace_project} or carrying a Nucleus label`}>
              <BarList
                rows={agents.map((a) => ({
                  key: a.agent,
                  label: a.agent,
                  sub: `${a.sessions} sessions · last ${a.last_day ?? "—"}${a.agent === "unlabeled" ? " · interactive, or a bot session whose label rotated out" : ""}`,
                  segments: [{ value: metricOf(metric, a.totals), color: COLOR.claude, label: formatMetric(metric, metricOf(metric, a.totals)) }],
                  value: formatMetric(metric, metricOf(metric, a.totals)),
                }))}
              />
            </Panel>
            <Panel label="by reminder" hint="skill-fire sessions">
              <Table
                head={["#", "reminder", "cron", "status", "fires", "range $", "30d fires", "30d $", "last day"]}
                align={["r", "l", "l", "l", "r", "r", "r", "r", "r"]}
                rows={reminders.map((r) => [
                  r.reminder_id,
                  r.title ?? "—",
                  <code className={faint}>{r.cron ?? "one-shot"}</code>,
                  <span className={faint}>{r.status ?? "deleted"}</span>,
                  r.sessions,
                  <MetricValue metric="cost" totals={r.totals} />,
                  r.sessions_30d,
                  formatUsd(r.cost_30d),
                  r.last_day ?? "—",
                ])}
              />
            </Panel>
          </>
        );
      }}
    </Loading>
  );
}

// ─── models ────────────────────────────────────────────────────────────────

function Models({ view, status }: { view: View; status: UsageStatus | null }) {
  const { metric, vendor } = view;
  const s = useFetch((sig) => getUsageSummary(view.days, vendor, sig), [view.days, vendor, view.version]);
  return (
    <>
      <Loading state={s}>
        {(d) => (
          <Panel label={`by model · ${scopeLabel(vendor)}`}>
            <BarList
              rows={d.models.map((m) => ({
                key: `${m.vendor}/${m.model}`,
                label: m.model,
                sub: `${m.vendor} · ${m.sessions} sessions`,
                segments: [
                  {
                    value: metricOf(metric, m.totals),
                    color: m.vendor === "codex" ? COLOR.codex : COLOR.claude,
                    label: formatMetric(metric, metricOf(metric, m.totals)),
                  },
                ],
                value:
                  metric === "cost" && m.totals.unpriced_tokens > 0
                    ? `no price (${formatTokens(m.totals.unpriced_tokens)} tokens)`
                    : formatMetric(metric, metricOf(metric, m.totals)),
              }))}
            />
            <div className="mt-5">
              <Table
                head={["vendor", "model", "uncached in", "cache write", "cache read", "output", "cache share", "$"]}
                align={["l", "l", "r", "r", "r", "r", "r", "r"]}
                rows={d.models.map((m) => {
                  const h = cacheHitRatio(m.totals);
                  return [
                    m.vendor,
                    m.model,
                    formatTokens(m.totals.input),
                    formatTokens(m.totals.cache_write),
                    formatTokens(m.totals.cache_read),
                    formatTokens(m.totals.output),
                    h === null ? "—" : `${(h * 100).toFixed(1)}%`,
                    <MetricValue metric="cost" totals={m.totals} />,
                  ];
                })}
              />
            </div>
          </Panel>
        )}
      </Loading>
      {status && (
        <Panel label="price table" hint={`USD per million tokens · built-in prices as of ${status.prices_as_of}`}>
          <Table
            head={["model", "priced as", "source", "input", "output", "cache read", "write 5m", "write 1h"]}
            align={["l", "l", "l", "r", "r", "r", "r", "r"]}
            rows={status.prices.map((p) => [
              p.model,
              p.matched_key ?? <span className="text-[var(--color-status-warn)]">[NO PRICE]</span>,
              <span className={faint}>{p.source ?? "—"}</span>,
              p.input ?? "—",
              p.output ?? "—",
              p.cache_read ?? "—",
              p.cache_write_5m ?? "—",
              p.cache_write_1h ?? "—",
            ])}
          />
          <p className={`mt-3 max-w-3xl text-xs leading-relaxed ${faint}`}>
            Claude sessions: where Claude Code recorded its own cost estimate (cost-state), that estimate is used, and the
            table prices only the tokens it did not cover. Claude Code estimates total {formatUsd(status.cost_state_usd)};
            the table differs from them by {formatUsd(status.cost_state_adjustment_usd)} on the same tokens. Cost-state also
            counted {formatTokens(status.residual_tokens)} tokens that no transcript line records (background calls); they
            are included. A model without a price is counted in tokens and flagged next to every dollar figure it belongs
            to. Override or add prices under <code>[usage.prices]</code> in nucleus.toml.
          </p>
        </Panel>
      )}
    </>
  );
}

// ─── limits ────────────────────────────────────────────────────────────────

const LANES = [
  { lane: "usage limit", color: "var(--color-status-down)" },
  { lane: "overloaded/5xx", color: "var(--color-status-warn)" },
  { lane: "other errors", color: "var(--color-nucleus-faint)" },
];

function laneOf(kind: string, status: number | null): string {
  if (kind === "rate_limit" || status === 429) return "usage limit";
  if (kind === "server_error" || (status !== null && status >= 500)) return "overloaded/5xx";
  return "other errors";
}

function Limits({ view }: { view: View }) {
  const { days, vendor } = view;
  const l = useFetch((sig) => getUsageLimits(days, vendor, sig), [days, vendor, view.version]);
  return (
    <Loading state={l}>
      {(d) => {
        const today = new Date().toLocaleDateString("en-CA");
        const firstEvent = d.events.length ? d.events[d.events.length - 1].day : today;
        const from = days === 0 ? firstEvent : addDays(today, -(days - 1));
        const dayList: string[] = [];
        for (let x = from; x <= today; x = addDays(x, 1)) dayList.push(x);
        const weekly = d.codex_daily.filter((p) => p.slot === "primary").map((p) => ({ day: p.day, value: p.max_used_percent }));
        return (
          <>
            <Panel label={`limit and error timeline · ${scopeLabel(vendor)}`} hint={`${d.events.length} events`}>
              <Timeline
                days={dayList}
                lanes={LANES}
                events={d.events.map((e, i) => ({
                  key: `${e.ts}-${i}`,
                  day: e.day,
                  lane: laneOf(e.kind, e.status),
                  body: (
                    <span>
                      {new Date(e.ts).toLocaleTimeString()} · {e.vendor} · {e.project ?? "?"}
                      {e.limit_type && ` · ${e.limit_type}`}
                    </span>
                  ),
                }))}
              />
            </Panel>
            <Panel label="Codex weekly limit, daily maximum" hint="as recorded in the Codex logs">
              {vendor === "claude" ? (
                <NotApplicable>Not applicable to the Claude filter: Claude transcripts record no quota reading.</NotApplicable>
              ) : weekly.length > 0 ? (
                <PercentLine points={weekly} />
              ) : (
                <NotApplicable>No Codex limit reading in this range.</NotApplicable>
              )}
            </Panel>
            <Panel label="events">
              <Table
                head={["time", "tool", "project", "agent", "kind", "status", "limit", "resets", "message"]}
                align={["l", "l", "l", "l", "l", "r", "l", "l", "l"]}
                rows={d.events.map((e) => [
                  new Date(e.ts).toLocaleString(),
                  e.vendor,
                  e.project ?? "—",
                  e.agent ?? "—",
                  e.kind,
                  e.status ?? "—",
                  e.limit_type ?? "—",
                  e.resets_at ? new Date(e.resets_at).toLocaleString() : "—",
                  <span className={faint}>{e.message ?? ""}</span>,
                ])}
              />
            </Panel>
          </>
        );
      }}
    </Loading>
  );
}

// ─── sessions ──────────────────────────────────────────────────────────────

function Sessions({ view }: { view: View }) {
  const { days, vendor } = view;
  const s = useFetch((sig) => getUsageSessions(days, 50, vendor, sig), [days, vendor, view.version]);
  return (
    <Loading state={s}>
      {(rows) => (
        <Panel label={`largest sessions · ${scopeLabel(vendor)}`} hint="by estimated cost">
          <Table
            head={["session", "project", "agent", "span", "tokens", "sub-agents", "$", "transcript"]}
            align={["l", "l", "l", "l", "r", "r", "r", "l"]}
            rows={rows.map((r) => [
              <div className="max-w-[22rem]">
                <div className="truncate">{r.title ?? r.reminder_title ?? <span className={faint}>untitled</span>}</div>
                <CopyText
                  text={r.vendor === "codex" ? `codex resume ${r.session_id}` : `claude --resume ${r.session_id}`}
                  label={`${r.vendor} · ${r.session_id.slice(0, 8)}`}
                />
              </div>,
              r.project ?? "—",
              r.reminder_title ? `${r.agent} · ${r.reminder_title}` : (r.agent ?? "—"),
              <span className={faint}>
                {new Date(r.first_ts).toLocaleDateString()} → {new Date(r.last_ts).toLocaleDateString()}
              </span>,
              formatTokens(r.totals.tokens),
              r.subagents,
              <MetricValue metric="cost" totals={r.totals} />,
              r.transcript_path ? (
                r.transcript_exists ? (
                  <CopyText text={r.transcript_path} label="copy path" />
                ) : (
                  <span className={faint}>deleted</span>
                )
              ) : (
                "—"
              ),
            ])}
          />
          <p className={`mt-3 text-xs ${faint}`}>
            Claude sessions of this workspace are searchable with <code>nucleus session-search</code>.
          </p>
        </Panel>
      )}
    </Loading>
  );
}
