import { useEffect, useLayoutEffect, useRef, useState, type ReactNode } from "react";
import { heatStep, niceTicks, type Metric, formatMetric, formatTick, type StackedBucket } from "@/lib/usage";

// Hand-rolled SVG charts for the usage surface (ADR-034). Palette is the
// locked dashboard palette: Claude = the amber accent, Codex = the faint
// neutral. Identity never rides on color alone — every two-series chart
// carries a legend, and every view has a table next to it.
//
// Mark specs (dataviz skill): bars ≤ 24px, 4px rounded data end, square at
// the baseline, 2px surface gap between stacked segments, hairline solid
// gridlines, text in text tokens only.

export const COLOR = {
  claude: "var(--color-nucleus-accent)",
  codex: "var(--color-nucleus-faint)",
  grid: "var(--color-nucleus-border)",
  surface: "var(--color-nucleus-surface)",
  text: "var(--color-nucleus-text)",
  faint: "var(--color-nucleus-faint)",
} as const;

/** Sequential amber ramp (one hue, surface → accent) for the heatmap. */
export const HEAT = [
  "var(--color-nucleus-bg)",
  "color-mix(in srgb, var(--color-nucleus-accent) 16%, var(--color-nucleus-surface))",
  "color-mix(in srgb, var(--color-nucleus-accent) 34%, var(--color-nucleus-surface))",
  "color-mix(in srgb, var(--color-nucleus-accent) 54%, var(--color-nucleus-surface))",
  "color-mix(in srgb, var(--color-nucleus-accent) 76%, var(--color-nucleus-surface))",
  "var(--color-nucleus-accent)",
];

/** Container width, tracked with a ResizeObserver. */
export function useWidth<T extends HTMLElement>(): [React.RefObject<T | null>, number] {
  const ref = useRef<T>(null);
  const [w, setW] = useState(0);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    setW(el.clientWidth);
    const ro = new ResizeObserver((entries) => setW(entries[0].contentRect.width));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  return [ref, w];
}

type Tip = { x: number; y: number; body: ReactNode } | null;

/** One tooltip per chart, positioned inside the chart's relative wrapper. */
export function useTooltip() {
  const [tip, setTip] = useState<Tip>(null);
  const wrap = useRef<HTMLDivElement>(null);
  const show = (e: React.PointerEvent | React.FocusEvent, body: ReactNode) => {
    const box = wrap.current?.getBoundingClientRect();
    if (!box) return;
    let x: number;
    let y: number;
    if ("clientX" in e) {
      x = e.clientX - box.left;
      y = e.clientY - box.top;
    } else {
      const t = (e.target as Element).getBoundingClientRect();
      x = t.left + t.width / 2 - box.left;
      y = t.top - box.top;
    }
    setTip({ x, y, body });
  };
  const hide = () => setTip(null);
  const el = tip ? (
    <div
      className="pointer-events-none absolute z-20 min-w-[9rem] rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-2.5 py-1.5 text-xs shadow-lg"
      style={{
        left: Math.min(tip.x + 12, (wrap.current?.clientWidth ?? 0) - 170),
        top: Math.max(tip.y - 12, 0),
        transform: "translateY(-100%)",
      }}
    >
      {tip.body}
    </div>
  ) : null;
  return { wrap, show, hide, el };
}

/** Tooltip row: short line key, value first (strong), label second. */
export function TipRow({ color, value, label }: { color?: string; value: string; label: string }) {
  return (
    <div className="flex items-center gap-2 whitespace-nowrap">
      {color ? <span className="inline-block h-[2px] w-3" style={{ background: color }} /> : <span className="w-3" />}
      <span className="text-[var(--color-nucleus-text)]">{value}</span>
      <span className="text-[var(--color-nucleus-faint)]">{label}</span>
    </div>
  );
}

export function Legend({ items }: { items: { label: string; color: string }[] }) {
  return (
    <div className="flex flex-wrap items-center gap-4 text-xs text-[var(--color-nucleus-faint)]">
      {items.map((i) => (
        <span key={i.label} className="flex items-center gap-1.5">
          <span className="inline-block h-2.5 w-2.5 rounded-[2px]" style={{ background: i.color }} />
          {i.label}
        </span>
      ))}
    </div>
  );
}

/** Rect with a 4px rounded top (data end) and a square bottom (baseline). */
function topRounded(x: number, y: number, w: number, h: number, r = 4): string {
  const rr = Math.min(r, w / 2, h);
  return `M${x},${y + h} V${y + rr} Q${x},${y} ${x + rr},${y} H${x + w - rr} Q${x + w},${y} ${x + w},${y + rr} V${y + h} Z`;
}

function shortDay(d: string) {
  return d.slice(5);
}

/** Two-series stacked columns (Claude bottom, Codex top) over buckets. */
export function StackedColumns({
  rows,
  metric,
  height = 190,
  bucketLabel = (b: string) => b,
}: {
  rows: StackedBucket[];
  metric: Metric;
  height?: number;
  bucketLabel?: (b: string) => string;
}) {
  const [ref, width] = useWidth<HTMLDivElement>();
  const tip = useTooltip();
  const padL = 56;
  const padB = 22;
  const padT = 8;
  const plotW = Math.max(0, width - padL - 4);
  const plotH = height - padB - padT;
  const max = Math.max(0, ...rows.map((r) => r.claude + r.codex));
  const ticks = niceTicks(max);
  const top = ticks[ticks.length - 1] || 1;
  const n = Math.max(rows.length, 1);
  const slot = plotW / n;
  const barW = Math.max(2, Math.min(24, slot * 0.72));
  const y = (v: number) => padT + plotH - (v / top) * plotH;
  const labelEvery = Math.max(1, Math.ceil(n / Math.max(1, Math.floor(plotW / 64))));

  return (
    <div ref={(el) => { ref.current = el; tip.wrap.current = el; }} className="relative w-full">
      {width > 0 && (
        <svg width={width} height={height} role="img" aria-label="usage per period, Claude and Codex stacked">
          {ticks.map((t) => (
            <g key={t}>
              <line x1={padL} x2={width - 4} y1={y(t)} y2={y(t)} stroke={COLOR.grid} strokeWidth={1} />
              <text x={padL - 8} y={y(t) + 4} textAnchor="end" fontSize={10} fill={COLOR.faint}>
                {formatTick(metric, t)}
              </text>
            </g>
          ))}
          {rows.map((r, i) => {
            const cx = padL + slot * i + slot / 2;
            const x = cx - barW / 2;
            const hClaude = (r.claude / top) * plotH;
            const hCodex = (r.codex / top) * plotH;
            const gap = hClaude > 0 && hCodex > 0 ? 2 : 0;
            const base = padT + plotH;
            const body = (
              <>
                <div className="mb-1 text-[var(--color-nucleus-faint)]">{bucketLabel(r.bucket)}</div>
                <TipRow color={COLOR.claude} value={formatMetric(metric, r.claude)} label="claude" />
                <TipRow color={COLOR.codex} value={formatMetric(metric, r.codex)} label="codex" />
                <TipRow value={formatMetric(metric, r.claude + r.codex)} label="total" />
              </>
            );
            return (
              <g key={r.bucket}>
                {hClaude > 0 &&
                  (hCodex > 0 ? (
                    <rect x={x} y={base - hClaude} width={barW} height={hClaude} fill={COLOR.claude} />
                  ) : (
                    <path d={topRounded(x, base - hClaude, barW, hClaude)} fill={COLOR.claude} />
                  ))}
                {hCodex > 0 && (
                  <path
                    d={topRounded(x, base - hClaude - gap - hCodex, barW, Math.max(hCodex, 1))}
                    fill={COLOR.codex}
                  />
                )}
                <rect
                  x={padL + slot * i}
                  y={padT}
                  width={slot}
                  height={plotH}
                  fill="transparent"
                  tabIndex={0}
                  className="outline-none hover:fill-[color-mix(in_srgb,var(--color-nucleus-text)_5%,transparent)] focus:fill-[color-mix(in_srgb,var(--color-nucleus-text)_5%,transparent)]"
                  onPointerMove={(e) => tip.show(e, body)}
                  onPointerLeave={tip.hide}
                  onFocus={(e) => tip.show(e, body)}
                  onBlur={tip.hide}
                />
                {i % labelEvery === 0 && (
                  <text x={cx} y={height - 6} textAnchor="middle" fontSize={10} fill={COLOR.faint}>
                    {shortDay(r.bucket)}
                  </text>
                )}
              </g>
            );
          })}
          <line x1={padL} x2={width - 4} y1={padT + plotH} y2={padT + plotH} stroke={COLOR.faint} strokeWidth={1} />
        </svg>
      )}
      {tip.el}
    </div>
  );
}

const DOW = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];

/** Hour-of-day × weekday heatmap, one amber hue, five steps of the max. */
export function Heatmap({ grid, metric }: { grid: number[][]; metric: Metric }) {
  const [ref, width] = useWidth<HTMLDivElement>();
  const tip = useTooltip();
  const padL = 36;
  const padT = 16;
  const gap = 2;
  const cell = Math.max(8, Math.min(26, (width - padL) / 24 - gap));
  const max = Math.max(0, ...grid.flat());
  const height = padT + 7 * (cell + gap);
  const total = grid.flat().reduce((a, b) => a + b, 0);
  return (
    <div>
      <div ref={(el) => { ref.current = el; tip.wrap.current = el; }} className="relative w-full overflow-x-auto">
        {width > 0 && (
          <svg width={padL + 24 * (cell + gap)} height={height} role="img" aria-label="usage by weekday and hour">
            {Array.from({ length: 24 }, (_, h) =>
              h % 3 === 0 ? (
                <text key={h} x={padL + h * (cell + gap)} y={10} fontSize={10} fill={COLOR.faint}>
                  {String(h).padStart(2, "0")}
                </text>
              ) : null,
            )}
            {grid.map((row, d) => (
              <g key={d}>
                <text x={0} y={padT + d * (cell + gap) + cell * 0.72} fontSize={10} fill={COLOR.faint}>
                  {DOW[d]}
                </text>
                {row.map((v, h) => {
                  const body = (
                    <>
                      <div className="mb-1 text-[var(--color-nucleus-faint)]">
                        {DOW[d]} {String(h).padStart(2, "0")}:00–{String(h).padStart(2, "0")}:59
                      </div>
                      <TipRow value={formatMetric(metric, v)} label={total > 0 ? `${((v / total) * 100).toFixed(1)}% of range` : ""} />
                    </>
                  );
                  return (
                    <rect
                      key={h}
                      x={padL + h * (cell + gap)}
                      y={padT + d * (cell + gap)}
                      width={cell}
                      height={cell}
                      rx={2}
                      fill={HEAT[heatStep(v, max)]}
                      tabIndex={0}
                      className="outline-none hover:opacity-80 focus:opacity-80"
                      onPointerMove={(e) => tip.show(e, body)}
                      onPointerLeave={tip.hide}
                      onFocus={(e) => tip.show(e, body)}
                      onBlur={tip.hide}
                    />
                  );
                })}
              </g>
            ))}
          </svg>
        )}
        {tip.el}
      </div>
      <div className="mt-2 flex items-center gap-1.5 text-[10px] text-[var(--color-nucleus-faint)]">
        <span>0</span>
        {HEAT.map((c, i) => (
          <span key={i} className="inline-block h-2.5 w-4 rounded-[2px]" style={{ background: c }} />
        ))}
        <span>{formatMetric(metric, max)} (max hour-cell; steps of 20%)</span>
      </div>
    </div>
  );
}

export type BarSegment = { value: number; color: string; label: string };
export type BarRow = { key: string; label: ReactNode; sub?: ReactNode; segments: BarSegment[]; value: string };

/** Horizontal bar list: label | bar (segments with 2px gap) | value at the tip. */
export function BarList({ rows, max: maxProp }: { rows: BarRow[]; max?: number }) {
  const tip = useTooltip();
  const max = maxProp ?? Math.max(0, ...rows.map((r) => r.segments.reduce((a, s) => a + s.value, 0)));
  return (
    <div ref={tip.wrap} className="relative space-y-1.5">
      {rows.map((r) => {
        const body = (
          <>
            <div className="mb-1 text-[var(--color-nucleus-faint)]">{r.label}</div>
            {r.segments.map((s) => (
              <TipRow key={s.label} color={s.color} value={s.label} label="" />
            ))}
          </>
        );
        return (
          <div
            key={r.key}
            tabIndex={0}
            className="grid grid-cols-[minmax(8rem,14rem)_1fr_auto] items-center gap-3 rounded px-1 py-0.5 text-sm outline-none hover:bg-[color-mix(in_srgb,var(--color-nucleus-text)_4%,transparent)] focus:bg-[color-mix(in_srgb,var(--color-nucleus-text)_4%,transparent)]"
            onPointerMove={(e) => tip.show(e, body)}
            onPointerLeave={tip.hide}
            onFocus={(e) => tip.show(e, body)}
            onBlur={tip.hide}
          >
            <div className="min-w-0">
              <div className="truncate">{r.label}</div>
              {r.sub && <div className="truncate text-[10px] text-[var(--color-nucleus-faint)]">{r.sub}</div>}
            </div>
            <div className="flex h-2.5 items-center gap-[2px]">
              {r.segments.map((s, i) =>
                s.value > 0 && max > 0 ? (
                  <div
                    key={i}
                    className="h-full"
                    style={{
                      width: `${(s.value / max) * 100}%`,
                      minWidth: 2,
                      background: s.color,
                      borderRadius: i === r.segments.length - 1 || r.segments.slice(i + 1).every((x) => x.value <= 0) ? "0 4px 4px 0" : 0,
                    }}
                  />
                ) : null,
              )}
            </div>
            <div className="text-right text-xs text-[var(--color-nucleus-text)]">{r.value}</div>
          </div>
        );
      })}
      {tip.el}
    </div>
  );
}

export type TimelineEvent = { key: string; day: string; lane: string; body: ReactNode };

/** Events per day on lanes (one lane per event kind) across a day range. */
export function Timeline({
  days,
  lanes,
  events,
}: {
  days: string[];
  lanes: { lane: string; color: string }[];
  events: TimelineEvent[];
}) {
  const [ref, width] = useWidth<HTMLDivElement>();
  const tip = useTooltip();
  const padL = 110;
  const laneH = 22;
  const height = lanes.length * laneH + 20;
  const plotW = Math.max(0, width - padL - 8);
  const idx = new Map(days.map((d, i) => [d, i]));
  const x = (d: string) => padL + ((idx.get(d) ?? 0) + 0.5) * (plotW / Math.max(days.length, 1));
  const labelEvery = Math.max(1, Math.ceil(days.length / Math.max(1, Math.floor(plotW / 64))));
  // Several events on the same day and lane stack as one dot with a count.
  const groups = new Map<string, TimelineEvent[]>();
  for (const e of events) {
    const k = `${e.lane}|${e.day}`;
    groups.set(k, [...(groups.get(k) ?? []), e]);
  }
  return (
    <div ref={(el) => { ref.current = el; tip.wrap.current = el; }} className="relative w-full">
      {width > 0 && (
        <svg width={width} height={height} role="img" aria-label="limit and error events per day">
          {lanes.map((l, li) => (
            <g key={l.lane}>
              <line x1={padL} x2={width - 8} y1={li * laneH + 11} y2={li * laneH + 11} stroke={COLOR.grid} strokeWidth={1} />
              <text x={0} y={li * laneH + 15} fontSize={10} fill={COLOR.faint}>
                {l.lane}
              </text>
            </g>
          ))}
          {[...groups.entries()].map(([k, evs]) => {
            const li = lanes.findIndex((l) => l.lane === evs[0].lane);
            if (li < 0 || !idx.has(evs[0].day)) return null;
            const cy = li * laneH + 11;
            const cx = x(evs[0].day);
            const body = (
              <>
                <div className="mb-1 text-[var(--color-nucleus-faint)]">
                  {evs[0].day} · {evs.length} event{evs.length > 1 ? "s" : ""}
                </div>
                {evs.slice(0, 6).map((e) => (
                  <div key={e.key}>{e.body}</div>
                ))}
                {evs.length > 6 && <div className="text-[var(--color-nucleus-faint)]">+{evs.length - 6} more</div>}
              </>
            );
            return (
              <g key={k}>
                <circle cx={cx} cy={cy} r={Math.min(4 + evs.length, 9)} fill={lanes[li].color} stroke={COLOR.surface} strokeWidth={2} />
                <circle
                  cx={cx}
                  cy={cy}
                  r={12}
                  fill="transparent"
                  tabIndex={0}
                  className="outline-none"
                  onPointerMove={(e) => tip.show(e, body)}
                  onPointerLeave={tip.hide}
                  onFocus={(e) => tip.show(e, body)}
                  onBlur={tip.hide}
                />
              </g>
            );
          })}
          {days.map((d, i) =>
            i % labelEvery === 0 ? (
              <text key={d} x={x(d)} y={height - 4} textAnchor="middle" fontSize={10} fill={COLOR.faint}>
                {shortDay(d)}
              </text>
            ) : null,
          )}
        </svg>
      )}
      {tip.el}
    </div>
  );
}

/** Single-series percentage line (0–100) over days, value labeled at the end. */
export function PercentLine({ points, height = 120 }: { points: { day: string; value: number }[]; height?: number }) {
  const [ref, width] = useWidth<HTMLDivElement>();
  const tip = useTooltip();
  const [hover, setHover] = useState<number | null>(null);
  useEffect(() => setHover(null), [points]);
  const padL = 36;
  const padR = 44;
  const padT = 8;
  const padB = 18;
  const plotW = Math.max(0, width - padL - padR);
  const plotH = height - padT - padB;
  const n = points.length;
  const x = (i: number) => padL + (n <= 1 ? plotW / 2 : (i / (n - 1)) * plotW);
  const y = (v: number) => padT + plotH - (Math.min(v, 100) / 100) * plotH;
  const d = points.map((p, i) => `${i === 0 ? "M" : "L"}${x(i)},${y(p.value)}`).join(" ");
  const last = points[n - 1];
  return (
    <div ref={(el) => { ref.current = el; tip.wrap.current = el; }} className="relative w-full">
      {width > 0 && n > 0 && (
        <svg
          width={width}
          height={height}
          role="img"
          aria-label="Codex weekly limit usage, daily maximum"
          onPointerMove={(e) => {
            const box = (e.currentTarget as SVGSVGElement).getBoundingClientRect();
            const px = e.clientX - box.left;
            const i = n <= 1 ? 0 : Math.round(((px - padL) / plotW) * (n - 1));
            const ci = Math.max(0, Math.min(n - 1, i));
            setHover(ci);
            tip.show(e, (
              <>
                <div className="mb-1 text-[var(--color-nucleus-faint)]">{points[ci].day}</div>
                <TipRow color={COLOR.claude} value={`${points[ci].value.toFixed(0)}%`} label="max used" />
              </>
            ));
          }}
          onPointerLeave={() => {
            setHover(null);
            tip.hide();
          }}
        >
          {[0, 50, 100].map((t) => (
            <g key={t}>
              <line x1={padL} x2={padL + plotW} y1={y(t)} y2={y(t)} stroke={COLOR.grid} strokeWidth={1} />
              <text x={padL - 6} y={y(t) + 4} textAnchor="end" fontSize={10} fill={COLOR.faint}>
                {t}%
              </text>
            </g>
          ))}
          <path d={d} fill="none" stroke={COLOR.claude} strokeWidth={2} strokeLinejoin="round" strokeLinecap="round" />
          {hover !== null && (
            <line x1={x(hover)} x2={x(hover)} y1={padT} y2={padT + plotH} stroke={COLOR.faint} strokeWidth={1} />
          )}
          <circle cx={x(n - 1)} cy={y(last.value)} r={4} fill={COLOR.claude} stroke={COLOR.surface} strokeWidth={2} />
          <text x={x(n - 1) + 8} y={y(last.value) + 4} fontSize={10} fill={COLOR.text}>
            {last.value.toFixed(0)}%
          </text>
          <text x={padL} y={height - 4} fontSize={10} fill={COLOR.faint}>
            {shortDay(points[0].day)}
          </text>
          <text x={padL + plotW} y={height - 4} fontSize={10} textAnchor="end" fill={COLOR.faint}>
            {shortDay(last.day)}
          </text>
        </svg>
      )}
      {tip.el}
    </div>
  );
}
