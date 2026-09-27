import { useState } from "react";
import { ShieldAlert } from "lucide-react";
import InlineConfirm from "@/components/InlineConfirm";
import Markdown from "@/components/Markdown";
import StatusPill from "@/components/StatusPill";
import { releaseItem, type WorkDetail, type WorkItem } from "@/lib/api";
import { canRelease, findingKindLabel, findingPlace, holdCode, markRanges, stageLabel, surfaceLabel } from "@/lib/work";
import { clockTime, shortId, shortTime, taskDuration, taskStatusKind } from "@/lib/tasks";
import { ActionButton, Field, Pre } from "./parts";

// Everything about an item besides the conversation and the plan: the
// hidden-content findings with the release action (first, when held), the
// source issue, the gate, the eval, the implementation and test output,
// the pull request link on the issue, the stage tasks and the stage log.

export default function ItemDetails({
  detail,
  now,
  busy,
  act,
}: {
  detail: WorkDetail;
  now: number;
  busy: boolean;
  act: (fn: () => Promise<WorkItem>) => Promise<boolean>;
}) {
  const { item, event, eval: ev, hidden, hidden_sources, tasks, transitions } = detail;
  const [confirmRelease, setConfirmRelease] = useState(false);
  // The hold this view shows; a release names it, so a newer hold refuses.
  const shownHold = item.hold_hash ?? "";

  return (
    <div className="space-y-5 px-4 py-4 text-xs md:px-5">
      {hidden.length > 0 && (
        <Field
          label={
            item.stage === "held"
              ? `held (hold ${holdCode(item.hold_hash)}): content GitHub's page does not show (${hidden.length})`
              : `hidden content (${hidden.length}), released via ${item.released_via ?? "?"} ${shortTime(item.released_at ?? "")}`
          }
        >
          <ul className="space-y-1.5">
            {hidden.map((f, i) => (
              <li key={i} className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-3 py-1.5">
                <div className="text-[10px] text-[var(--color-nucleus-faint)]">
                  {findingPlace(f)} · <span className="text-[var(--color-status-warn)]">{findingKindLabel(f.kind)}</span>
                </div>
                <div className="whitespace-pre-wrap break-all font-mono text-[var(--color-nucleus-text)] [font-variant-ligatures:none]">{f.text}</div>
              </li>
            ))}
          </ul>
          {hidden_sources.map((s) => (
            <div key={s.location} className="mt-2">
              <div className="mb-0.5 text-[10px] text-[var(--color-nucleus-faint)]">raw {s.location}, hidden parts marked</div>
              <pre className="max-h-80 overflow-auto whitespace-pre-wrap break-all rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-3 py-2 font-mono text-xs text-[var(--color-nucleus-text)] [font-variant-ligatures:none]">
                {markRanges(
                  s.text,
                  hidden.filter((f) => f.location === s.location),
                ).map((p, i) =>
                  p.flagged ? (
                    <mark key={i} className="bg-[var(--color-status-warn)] text-[var(--color-nucleus-bg)]">
                      {p.text}
                    </mark>
                  ) : (
                    <span key={i}>{p.text}</span>
                  ),
                )}
              </pre>
            </div>
          ))}
          {canRelease(item) && !confirmRelease && (
            <div className="mt-2">
              <ActionButton onClick={() => setConfirmRelease(true)} disabled={busy}>
                <ShieldAlert size={11} strokeWidth={1.75} />
                release (hold {holdCode(shownHold)})
              </ActionButton>
            </div>
          )}
          {canRelease(item) && confirmRelease && (
            <InlineConfirm
              className="mt-2 px-0 py-2"
              message={`Release item #${item.id} (hold ${holdCode(shownHold)})? The agent reads this hidden content as data. Refused if the item was held again or the issue changed since.`}
              confirmLabel={busy ? "releasing…" : "release"}
              busy={busy}
              onConfirm={() => void act(() => releaseItem(item.id, shownHold)).then((ok) => ok && setConfirmRelease(false))}
              onCancel={() => setConfirmRelease(false)}
            />
          )}
        </Field>
      )}

      <Field label="source">
        <div className="flex flex-wrap gap-x-4 gap-y-0.5 text-[var(--color-nucleus-faint)]">
          <span>
            {event.source} <code className="text-[var(--color-nucleus-text)]">{event.external_id}</code>
          </span>
          {event.author && <span>by {event.author}</span>}
          {event.labels.length > 0 && <span>labels {event.labels.join(", ")}</span>}
          {event.url && (
            <a href={event.url} target="_blank" rel="noreferrer" className="text-[var(--color-nucleus-accent)] hover:underline">
              open
            </a>
          )}
          <span>thread: {surfaceLabel(item)}</span>
        </div>
        {event.body.trim() && <div className="mt-1.5"><Pre>{event.body}</Pre></div>}
      </Field>

      {item.gate_event_id && (
        <Field label="gate">
          <div className="text-[var(--color-nucleus-faint)]">
            {item.gate_event_id} by {item.gate_actor ?? "?"} at {item.gate_at ?? "?"}; bound to the issue text as it was then
          </div>
        </Field>
      )}

      {ev && (
        <Field label="eval">
          <div className="space-y-1 text-[var(--color-nucleus-text)]">
            <div>
              <span className="text-[var(--color-nucleus-accent)]">{ev.effective}</span>
              {ev.effective !== ev.classification && <span className="text-[var(--color-nucleus-faint)]"> (agent said {ev.classification})</span>} —{" "}
              {ev.summary}
            </div>
            <div className="text-[var(--color-nucleus-faint)]">
              size {ev.criteria.change_size} · schema {String(ev.criteria.schema_impact)} · security {String(ev.criteria.security_impact)} · public API{" "}
              {String(ev.criteria.public_api_impact)} · confidence {ev.criteria.confidence.toFixed(2)}
            </div>
            <ul className="list-inside list-disc">
              {ev.reasons.map((r, i) => (
                <li key={`r${i}`}>{r}</li>
              ))}
              {ev.escalations.map((r, i) => (
                <li key={`e${i}`} className="text-[var(--color-status-warn)]">
                  raised to complex: {r}
                </li>
              ))}
            </ul>
          </div>
        </Field>
      )}

      {item.impl_summary && (
        <Field label={`implementation · branch ${item.branch ?? "—"}`}>
          <Markdown source={item.impl_summary} />
        </Field>
      )}

      {item.tests_status && (
        <Field label={`tests (run by Nucleus): ${item.tests_status}`}>
          {item.tests_output ? <Pre>{item.tests_output}</Pre> : <span className="text-[var(--color-nucleus-faint)]">no output</span>}
        </Field>
      )}

      {item.pr_url && (
        <Field label="draft pull request">
          <a href={item.pr_url} target="_blank" rel="noreferrer" className="break-all text-[var(--color-nucleus-accent)] hover:underline">
            {item.pr_url}
          </a>
        </Field>
      )}

      {item.comment_state !== "none" && (
        <Field label="pull request link on the issue">
          {item.comment_state === "skipped" ? (
            <span className="text-[var(--color-nucleus-faint)]">not posted: the event's source has no reply channel</span>
          ) : item.comment_url ? (
            <a href={item.comment_url} target="_blank" rel="noreferrer" className="text-[var(--color-nucleus-accent)] hover:underline">
              posted comment
            </a>
          ) : (
            <span>posted</span>
          )}
        </Field>
      )}

      <Field label={`stage tasks (${tasks.length})`}>
        {tasks.length === 0 ? (
          <span className="text-[var(--color-nucleus-faint)]">none yet</span>
        ) : (
          <ul className="space-y-1">
            {tasks.map((t) => (
              <li key={t.id} className="flex flex-wrap items-center gap-x-2 gap-y-0.5">
                <code className="text-[var(--color-nucleus-faint)]" title={t.id}>
                  {shortId(t.id)}
                </code>
                <span className="text-[var(--color-nucleus-text)]">{t.kind}</span>
                <StatusPill kind={taskStatusKind(t.status)}>{t.status.toUpperCase()}</StatusPill>
                <span className="text-[var(--color-nucleus-faint)]">{taskDuration(t, now) ?? "not started"}</span>
                <span className="text-[var(--color-nucleus-faint)]">{t.profile}</span>
              </li>
            ))}
          </ul>
        )}
      </Field>

      <Field label="stages">
        <ul className="space-y-1">
          {transitions.map((t) => (
            <li key={t.id} className="flex flex-wrap gap-x-2">
              <span className="shrink-0 text-[var(--color-nucleus-faint)]" title={t.at}>
                {clockTime(t.at)}
              </span>
              <span className="shrink-0 text-[var(--color-nucleus-accent)]">
                {t.from_stage ? stageLabel(t.from_stage) : "—"} → {stageLabel(t.to_stage)}
              </span>
              <span className="min-w-0 basis-full break-words text-[var(--color-nucleus-text)] sm:basis-auto sm:flex-1">{t.reason}</span>
            </li>
          ))}
        </ul>
      </Field>
    </div>
  );
}
