import { useEffect, useState, type ReactNode } from "react";
import InlineConfirm from "@/components/InlineConfirm";
import StatusPill from "@/components/StatusPill";
import { useFetch } from "@/lib/hooks";
import {
  approveComment,
  approvePlan,
  getIntakeDetail,
  replyToItem,
  skipComment,
  type IntakeItem,
} from "@/lib/api";
import {
  authorLabel,
  canApprovePlan,
  canDecideComment,
  canReply,
  surfaceLabel,
  threadOrder,
} from "@/lib/intake";
import { clockTime, shortId, shortTime, taskDuration, taskStatusKind } from "@/lib/tasks";

// Expanded view of one pipeline item: the source event, the eval, the plan
// (with its approval), the thread with a reply box, the implementation and
// test result, the pull request and the proposed issue comment, the stage
// tasks and the stage log. Refetched whenever `version` changes (the list
// row derives it from the item, so a list refresh that changes the item
// refreshes this panel too).

type Confirm = "plan" | "comment" | "skip" | null;

export default function ItemDetailPanel({
  itemId,
  version,
  now,
  onChange,
}: {
  itemId: number;
  version: string;
  now: number;
  onChange: (item: IntakeItem) => void;
}) {
  const detail = useFetch((signal) => getIntakeDetail(itemId, signal), [itemId, version]);
  const [reply, setReply] = useState("");
  const [comment, setComment] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<Confirm>(null);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  const item = detail.data?.item;
  // The comment editor starts from the proposed text each time a new
  // proposal arrives.
  useEffect(() => setComment(item?.comment_draft ?? null), [item?.comment_draft]);

  if (detail.error && !detail.data) {
    return <div className="border-t border-[var(--color-nucleus-border)] px-4 py-3 text-xs text-[var(--color-status-down)]">{detail.error}</div>;
  }
  if (!detail.data || !item) {
    return <div className="border-t border-[var(--color-nucleus-border)] px-4 py-3 text-xs text-[var(--color-nucleus-faint)]">fetching…</div>;
  }
  const { event, eval: ev, tasks, transitions } = detail.data;
  const messages = threadOrder(detail.data.messages);

  const act = async (fn: () => Promise<IntakeItem>, after?: () => void) => {
    setBusy(true);
    setErr(null);
    try {
      onChange(await fn());
      after?.();
      setConfirm(null);
      detail.refetch();
    } catch (e) {
      setErr(String(e instanceof Error ? e.message : e));
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="space-y-4 border-t border-[var(--color-nucleus-border)] px-4 py-3 text-xs">
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
        {event.body.trim() && <Pre>{event.body}</Pre>}
      </Field>

      {err && <div className="text-[var(--color-status-down)]">{err}</div>}
      {item.stale_reason ? (
        <Field label="stale">
          <Pre tone="text-[var(--color-status-down)]">
            {`${item.stale_reason}\nNothing more is done for this item. Remove and add the label again on the issue to start a new item from its current text.`}
          </Pre>
        </Field>
      ) : (
        item.error && (
          <Field
            label={
              item.stage === "failed"
                ? `failed in ${item.failed_stage ?? "?"}`
                : item.stage === "blocked"
                  ? `blocked in ${item.failed_stage ?? "?"}`
                  : "last error"
            }
          >
            <Pre tone="text-[var(--color-status-down)]">{item.error}</Pre>
          </Field>
        )
      )}
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
              {ev.effective !== ev.classification && (
                <span className="text-[var(--color-nucleus-faint)]"> (agent said {ev.classification})</span>
              )}{" "}
              — {ev.summary}
            </div>
            <div className="text-[var(--color-nucleus-faint)]">
              size {ev.criteria.change_size} · schema {String(ev.criteria.schema_impact)} · security{" "}
              {String(ev.criteria.security_impact)} · public API {String(ev.criteria.public_api_impact)} · confidence{" "}
              {ev.criteria.confidence.toFixed(2)}
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

      {(item.approved_plan || item.plan_draft) && (
        <Field
          label={
            item.approved_plan
              ? `approved plan v${item.approved_version} (${item.approved_via} ${shortTime(item.approved_at ?? "")})`
              : `proposed plan v${item.plan_version}`
          }
        >
          <Pre>{item.approved_plan ?? item.plan_draft ?? ""}</Pre>
          {canApprovePlan(item) && confirm !== "plan" && (
            <ActionButton onClick={() => setConfirm("plan")} disabled={busy}>
              approve plan v{item.plan_version}
            </ActionButton>
          )}
        </Field>
      )}
      {confirm === "plan" && (
        <InlineConfirm
          className="px-0 py-2"
          message={`Approve plan v${item.plan_version}? The implementation agent starts from it.`}
          confirmLabel={busy ? "approving…" : "approve"}
          busy={busy}
          onConfirm={() => void act(() => approvePlan(item.id, item.plan_version))}
          onCancel={() => setConfirm(null)}
        />
      )}

      <Field label={`thread (${messages.length})`}>
        {messages.length === 0 ? (
          <span className="text-[var(--color-nucleus-faint)]">no messages</span>
        ) : (
          <ul className="space-y-2">
            {messages.map((m) => (
              <li key={m.id} className={m.author === "operator" ? "border-l-2 border-[var(--color-nucleus-accent)] pl-2" : "pl-2.5"}>
                <div className="text-[10px] text-[var(--color-nucleus-faint)]" title={m.at}>
                  {authorLabel(m)} · {shortTime(m.at)}
                  {m.pending_agent === 1 && " · not read by the agent yet"}
                </div>
                <div className="whitespace-pre-wrap break-words text-[var(--color-nucleus-text)]">{m.body}</div>
              </li>
            ))}
          </ul>
        )}
        {canReply(item) && (
          <form
            className="mt-3 flex flex-col gap-2"
            onSubmit={(e) => {
              e.preventDefault();
              if (reply.trim()) void act(() => replyToItem(item.id, reply), () => setReply(""));
            }}
          >
            <textarea
              value={reply}
              onChange={(e) => setReply(e.target.value)}
              rows={3}
              placeholder="reply to the refinement agent (also sent to the WhatsApp thread)"
              className="w-full rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-2 py-1.5 font-mono text-xs text-[var(--color-nucleus-text)] outline-none focus:border-[var(--color-nucleus-accent)]"
            />
            <div>
              <ActionButton type="submit" disabled={busy || !reply.trim()}>
                {busy ? "sending…" : "send"}
              </ActionButton>
            </div>
          </form>
        )}
      </Field>

      {item.impl_summary && (
        <Field label={`implementation · branch ${item.branch ?? "—"}`}>
          <Pre>{item.impl_summary}</Pre>
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
        <Field label={`issue comment: ${item.comment_state}`}>
          {canDecideComment(item) ? (
            <textarea
              value={comment ?? ""}
              onChange={(e) => setComment(e.target.value)}
              rows={5}
              className="w-full rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-2 py-1.5 font-mono text-xs text-[var(--color-nucleus-text)] outline-none focus:border-[var(--color-nucleus-accent)]"
            />
          ) : (
            <Pre>{item.comment_draft ?? ""}</Pre>
          )}
          {item.comment_url && (
            <a href={item.comment_url} target="_blank" rel="noreferrer" className="text-[var(--color-nucleus-accent)] hover:underline">
              posted comment
            </a>
          )}
          {canDecideComment(item) && confirm === null && (
            <div className="mt-2 flex gap-2">
              <ActionButton onClick={() => setConfirm("comment")} disabled={busy || !(comment ?? "").trim()}>
                approve and post
              </ActionButton>
              <ActionButton onClick={() => setConfirm("skip")} disabled={busy}>
                post nothing
              </ActionButton>
            </div>
          )}
        </Field>
      )}
      {confirm === "comment" && (
        <InlineConfirm
          className="px-0 py-2"
          message="Post this comment on the issue? The issue is public."
          confirmLabel={busy ? "posting…" : "post"}
          busy={busy}
          onConfirm={() =>
            void act(() => approveComment(item.id, comment !== item.comment_draft ? (comment ?? undefined) : undefined))
          }
          onCancel={() => setConfirm(null)}
        />
      )}
      {confirm === "skip" && (
        <InlineConfirm
          className="px-0 py-2"
          message="Close the item without a comment on the issue?"
          confirmLabel={busy ? "closing…" : "close without comment"}
          busy={busy}
          onConfirm={() => void act(() => skipComment(item.id))}
          onCancel={() => setConfirm(null)}
        />
      )}

      <Field label={`stage tasks (${tasks.length})`}>
        {tasks.length === 0 ? (
          <span className="text-[var(--color-nucleus-faint)]">none yet</span>
        ) : (
          <ul className="space-y-0.5">
            {tasks.map((t) => (
              <li key={t.id} className="flex flex-wrap items-center gap-2">
                <code className="text-[var(--color-nucleus-faint)]" title={t.id}>
                  {shortId(t.id)}
                </code>
                <span className="w-32 text-[var(--color-nucleus-text)]">{t.kind}</span>
                <StatusPill kind={taskStatusKind(t.status)}>{t.status.toUpperCase()}</StatusPill>
                <span className="text-[var(--color-nucleus-faint)]">{taskDuration(t, now) ?? "not started"}</span>
                <span className="text-[var(--color-nucleus-faint)]">{t.profile}</span>
              </li>
            ))}
          </ul>
        )}
      </Field>

      <Field label="stages">
        <ul className="space-y-0.5">
          {transitions.map((t) => (
            <li key={t.id} className="flex gap-2">
              <span className="shrink-0 text-[var(--color-nucleus-faint)]" title={t.at}>
                {clockTime(t.at)}
              </span>
              <span className="w-40 shrink-0 text-[var(--color-nucleus-accent)]">
                {t.from_stage ?? "—"} → {t.to_stage}
              </span>
              <span className="min-w-0 flex-1 break-words text-[var(--color-nucleus-text)]">{t.reason}</span>
            </li>
          ))}
        </ul>
      </Field>
    </div>
  );
}

function ActionButton({
  children,
  onClick,
  disabled,
  type = "button",
}: {
  children: ReactNode;
  onClick?: () => void;
  disabled?: boolean;
  type?: "button" | "submit";
}) {
  return (
    <button
      type={type}
      onClick={onClick}
      disabled={disabled}
      className="mt-2 rounded border border-[var(--color-nucleus-border)] px-2 py-0.5 text-[var(--color-nucleus-faint)] transition-colors hover:border-[var(--color-nucleus-accent)] hover:text-[var(--color-nucleus-accent)] disabled:opacity-40"
    >
      {children}
    </button>
  );
}

function Field({ label, children }: { label: string; children: ReactNode }) {
  return (
    <section>
      <div className="mb-1 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)] opacity-70">{label}</div>
      {children}
    </section>
  );
}

function Pre({ children, tone = "text-[var(--color-nucleus-text)]" }: { children: string; tone?: string }) {
  return (
    <pre
      className={`max-h-80 overflow-auto whitespace-pre-wrap break-words rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-3 py-2 font-mono text-xs ${tone}`}
    >
      {children}
    </pre>
  );
}
