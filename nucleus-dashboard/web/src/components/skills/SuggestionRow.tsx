import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { Check, ChevronDown, ChevronRight, FilePen, FilePlus2, Pencil, X } from "lucide-react";
import DiffView, { DiffCount } from "@/components/DiffView";
import InlineConfirm from "@/components/InlineConfirm";
import Markdown from "@/components/Markdown";
import StatusPill from "@/components/StatusPill";
import ViewTab from "@/components/ViewTab";
import OutcomePanel, { type Outcome } from "@/components/skills/OutcomePanel";
import { useFetch } from "@/lib/hooks";
import {
  acceptSuggestion,
  editSuggestion,
  getSuggestion,
  rejectSuggestion,
  SkillActionError,
  type Suggestion,
  type SuggestionReview,
  type SuggestionSummary,
} from "@/lib/api";
import { lineDiff } from "@/lib/linediff";
import {
  acceptConfirmText,
  describeAccept,
  evidenceSourceLabel,
  kindLabel,
  liveSkillNote,
  oneLine,
  originLabel,
  refusalHead,
  shortDate,
  skillFilePath,
  splitFrontmatter,
  statusKind,
} from "@/lib/suggestions";

// One skill suggestion (ADR-038). Collapsed: the skill name, kind, origin,
// evidence count, date and the rationale on one line. Open: the draft
// SKILL.md rendered or raw, for a patch the changes against the live
// SKILL.md, the evidence with its quoted turns, and, while pending, the
// actions. Accept and reject each ask an inline second step (reject takes
// an optional reason there); edit replaces the draft in a text area. The
// server checks everything and its refusal is shown as it returns it.

export type View = "rendered" | "raw" | "changes";
export type Step = "none" | "accept" | "reject" | "edit";

export default function SuggestionRow({
  summary,
  open,
  onToggle,
  onOpen,
  onChanged,
  onLibraryChanged,
  onDecided,
}: {
  summary: SuggestionSummary;
  open: boolean;
  onToggle: () => void;
  /** Open another suggestion (a "superseded by #n" link). */
  onOpen: (id: number) => void;
  /** A decision or an edit changed the suggestion list. */
  onChanged: () => void;
  /** An accept wrote a skill: the library listing is stale. */
  onLibraryChanged: () => void;
  /** An accept or reject succeeded: the suggestion leaves the pending
   *  list, so the section shows the result. */
  onDecided: (o: Outcome) => void;
}) {
  const ref = useRef<HTMLElement>(null);
  // A suggestion open when the page loads came from a link: bring it into view.
  useEffect(() => {
    if (open) ref.current?.scrollIntoView({ block: "start" });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const KindIcon = summary.kind === "create" ? FilePlus2 : FilePen;
  return (
    <article
      ref={ref}
      data-suggestion={summary.id}
      className="scroll-mt-4 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)]"
    >
      <button
        onClick={onToggle}
        aria-expanded={open}
        className="flex w-full min-w-0 items-start gap-3 px-4 py-3 text-left transition-colors hover:bg-[var(--color-nucleus-bg)]"
      >
        {open ? (
          <ChevronDown size={14} strokeWidth={1.75} className="mt-1 shrink-0 text-[var(--color-nucleus-faint)]" />
        ) : (
          <ChevronRight size={14} strokeWidth={1.75} className="mt-1 shrink-0 text-[var(--color-nucleus-faint)]" />
        )}
        <KindIcon size={14} strokeWidth={1.75} className="mt-1 shrink-0 text-[var(--color-nucleus-accent)]" aria-label={summary.kind} />
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-baseline gap-x-2 gap-y-0.5">
            <span className="text-[var(--color-nucleus-faint)]">#{summary.id}</span>
            <span className="text-base text-[var(--color-nucleus-text)]">{summary.name}</span>
            <span className="text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)]">{kindLabel(summary.kind)}</span>
            {summary.status !== "pending" && <StatusPill kind={statusKind(summary.status)}>{summary.status.toUpperCase()}</StatusPill>}
            {summary.edited && <span className="text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)]">edited</span>}
          </div>
          <p className="mt-1 line-clamp-1 text-sm leading-relaxed text-[var(--color-nucleus-faint)]">{oneLine(summary.rationale, 220)}</p>
          <div className="mt-2 flex flex-wrap items-center gap-x-3 gap-y-0.5 text-[11px] text-[var(--color-nucleus-faint)]">
            <span>{originLabel(summary.origin)}</span>
            <span>
              {summary.evidence_count} {summary.evidence_count === 1 ? "session" : "sessions"} of evidence
            </span>
            <span>filed {shortDate(summary.created_at)}</span>
            {summary.status === "rejected" && summary.reject_reason && <span>rejected: {oneLine(summary.reject_reason, 120)}</span>}
            {summary.superseded_by !== null && <span>superseded by #{summary.superseded_by}</span>}
          </div>
        </div>
      </button>
      {open && (
        <SuggestionDetail id={summary.id} onOpen={onOpen} onChanged={onChanged} onLibraryChanged={onLibraryChanged} onDecided={onDecided} />
      )}
    </article>
  );
}

function SuggestionDetail({
  id,
  onOpen,
  onChanged,
  onLibraryChanged,
  onDecided,
}: {
  id: number;
  onOpen: (id: number) => void;
  onChanged: () => void;
  onLibraryChanged: () => void;
  onDecided: (o: Outcome) => void;
}) {
  const detail = useFetch((signal) => getSuggestion(id, signal), [id]);
  if (detail.error) {
    return <div className="border-t border-[var(--color-nucleus-border)] px-4 py-3 text-xs text-[var(--color-status-down)]">{detail.error}</div>;
  }
  if (!detail.data) {
    return <div className="border-t border-[var(--color-nucleus-border)] px-4 py-3 text-xs text-[var(--color-nucleus-faint)]">loading…</div>;
  }
  return (
    <SuggestionScreen
      review={detail.data}
      onOpen={onOpen}
      onChanged={() => {
        detail.refetch();
        onChanged();
      }}
      onLibraryChanged={onLibraryChanged}
      onDecided={onDecided}
    />
  );
}

/** The open suggestion, from its review. Exported for the component tests
 *  (static render, so `initialStep` and `initialView` set what they see). */
export function SuggestionScreen({
  review,
  onOpen,
  onChanged,
  onLibraryChanged,
  onDecided,
  initialStep = "none",
  initialView = "rendered",
}: {
  review: SuggestionReview;
  onOpen: (id: number) => void;
  /** After every write, accepted or refused: fetch the suggestion and the
   *  list again. */
  onChanged: () => void;
  onLibraryChanged: () => void;
  /** Receives the result of a successful accept or reject. */
  onDecided: (o: Outcome) => void;
  initialStep?: Step;
  initialView?: View;
}) {
  const [view, setView] = useState<View>(initialView);
  const [step, setStep] = useState<Step>(initialStep);
  const [busy, setBusy] = useState(false);
  const [outcome, setOutcome] = useState<Outcome | null>(null);
  const [reason, setReason] = useState("");
  const [draft, setDraft] = useState(initialStep === "edit" ? review.suggestion.draft : "");

  const s = review.suggestion;
  const live = review.live;
  const diff = useMemo(() => (live?.text != null ? lineDiff(live.text, s.draft) : null), [s.draft, live?.text]);

  const pending = s.status === "pending";

  /** Run a write; a refusal is shown with the server's text and any draft
   *  problems. The suggestion and the list are fetched again either way. */
  const run = async (fn: () => Promise<Outcome | null>): Promise<boolean> => {
    setBusy(true);
    setOutcome(null);
    try {
      const o = await fn();
      if (o) setOutcome(o);
      return true;
    } catch (e) {
      if (e instanceof SkillActionError) {
        setOutcome({ kind: "error", lines: [refusalHead(e.message, e.issues)], reminders: [], issues: e.issues });
      } else {
        setOutcome({ kind: "error", lines: [String(e)], reminders: [] });
      }
      return false;
    } finally {
      setBusy(false);
      onChanged();
    }
  };

  const accept = () =>
    void run(async () => {
      // Bound to the draft on screen: an edit or revision since is refused.
      const resp = await acceptSuggestion(s.id, review.draft_hash);
      onLibraryChanged();
      onDecided(describeAccept(resp));
      return null;
    }).then(() => setStep("none"));

  const reject = () =>
    void run(async () => {
      await rejectSuggestion(s.id, reason.trim() || null);
      onDecided({ kind: "ok", lines: [`rejected #${s.id} ${s.name}${reason.trim() ? `: ${reason.trim()}` : ""}`] });
      return null;
    }).then((ok) => ok && setStep("none"));

  const save = () =>
    void run(async () => {
      await editSuggestion(s.id, draft);
      return { kind: "ok", lines: [`saved the edited draft of #${s.id}; it passed the checks`] };
    }).then((ok) => ok && setStep("none"));

  const startEdit = () => {
    setDraft(s.draft);
    setOutcome(null);
    setStep("edit");
  };

  return (
    <div className="border-t border-[var(--color-nucleus-border)]">
      <div className="space-y-4 px-4 py-3">
        {outcome && step !== "edit" && <OutcomePanel outcome={outcome} onDismiss={() => setOutcome(null)} />}

        <Facts s={s} onOpen={onOpen} />

        {pending && review.accept_blockers.length > 0 && (
          <div role="alert" className="rounded border border-[var(--color-status-down)] px-3 py-2 text-xs">
            <div className="text-[var(--color-status-down)]">Accept is refused now:</div>
            <ul className="mt-1 list-disc space-y-0.5 pl-4 text-[var(--color-nucleus-text)]">
              {review.accept_blockers.map((b, k) => (
                <li key={k}>{b}</li>
              ))}
            </ul>
          </div>
        )}
        {live && <LiveNote review={review} />}

        {pending && step !== "edit" && (
          <div className="flex flex-wrap items-center gap-2">
            <SmallButton onClick={() => setStep("accept")} disabled={busy || step !== "none"}>
              <Check size={11} strokeWidth={2} />
              accept
            </SmallButton>
            <SmallButton onClick={() => setStep("reject")} disabled={busy || step !== "none"} danger>
              <X size={11} strokeWidth={2} />
              reject
            </SmallButton>
            <SmallButton onClick={startEdit} disabled={busy || step !== "none"}>
              <Pencil size={11} strokeWidth={1.75} />
              edit draft
            </SmallButton>
            {busy && <span className="text-[11px] text-[var(--color-nucleus-faint)]">working…</span>}
          </div>
        )}
      </div>

      {step === "accept" && (
        <InlineConfirm
          message={acceptConfirmText(s)}
          confirmLabel={busy ? "accepting…" : "accept"}
          busy={busy}
          onConfirm={accept}
          onCancel={() => setStep("none")}
        />
      )}
      {step === "reject" && (
        <RejectStrip
          id={s.id}
          reason={reason}
          onReason={setReason}
          busy={busy}
          onConfirm={reject}
          onCancel={() => setStep("none")}
        />
      )}

      <div className="border-t border-[var(--color-nucleus-border)] px-4 py-3">
        {step === "edit" ? (
          <EditDraft
            outcome={outcome}
            onDismiss={() => setOutcome(null)}
            draft={draft}
            onDraft={setDraft}
            busy={busy}
            onSave={save}
            onCancel={() => setStep("none")}
          />
        ) : (
          <>
            <div role="tablist" className="mb-3 flex flex-wrap items-center gap-1 text-xs">
              <span className="mr-2 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)] opacity-70">draft</span>
              <ViewTab active={view === "rendered"} onClick={() => setView("rendered")}>
                rendered
              </ViewTab>
              <ViewTab active={view === "raw"} onClick={() => setView("raw")}>
                raw
              </ViewTab>
              {s.kind === "patch" && (
                <ViewTab active={view === "changes"} onClick={() => setView("changes")} disabled={!diff}>
                  changes from the live skill
                </ViewTab>
              )}
              {diff && view === "changes" && <DiffCount rows={diff} />}
            </div>
            {view === "changes" && diff ? (
              <DiffView ops={diff} />
            ) : view === "raw" ? (
              <pre className="overflow-x-auto whitespace-pre-wrap rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-3 py-2 text-[12px] leading-relaxed text-[var(--color-nucleus-text)]">
                {s.draft}
              </pre>
            ) : (
              <RenderedDraft text={s.draft} />
            )}
          </>
        )}
      </div>

      <div className="border-t border-[var(--color-nucleus-border)] px-4 py-3">
        <Evidence s={s} />
      </div>
    </div>
  );
}

function Label({ children }: { children: ReactNode }) {
  return <div className="mb-1 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-faint)] opacity-70">{children}</div>;
}

/** Rationale and the suggestion's history. */
function Facts({ s, onOpen }: { s: Suggestion; onOpen: (id: number) => void }) {
  const link = (n: number) => (
    <button onClick={() => onOpen(n)} className="text-[var(--color-nucleus-accent)] hover:underline">
      #{n}
    </button>
  );
  return (
    <div className="space-y-3 text-sm">
      <section>
        <Label>rationale</Label>
        <p className="whitespace-pre-wrap leading-relaxed text-[var(--color-nucleus-text)]">{s.rationale}</p>
      </section>
      {s.overrides_rejected !== null && (
        <section>
          <Label>suggested again after rejection</Label>
          <p className="leading-relaxed text-[var(--color-nucleus-text)]">
            {link(s.overrides_rejected)} was rejected; what is new: {s.why_new ?? "(not given)"}
          </p>
        </section>
      )}
      <div className="flex flex-wrap gap-x-3 gap-y-0.5 text-[11px] text-[var(--color-nucleus-faint)]">
        <span>
          {kindLabel(s.kind)} · {originLabel(s.origin)} · filed {shortDate(s.created_at)}
        </span>
        <span>writes {skillFilePath(s.name)}</span>
        {s.edited && <span>draft edited by you</span>}
        {s.decided_at && (
          <span>
            {s.status} {shortDate(s.decided_at)}
          </span>
        )}
        {s.superseded_by !== null && <span>superseded by {link(s.superseded_by)}</span>}
        {s.reject_reason && <span>reason: {s.reject_reason}</span>}
      </div>
    </div>
  );
}

function LiveNote({ review }: { review: SuggestionReview }) {
  if (!review.live) return null;
  const note = liveSkillNote(review.live);
  const tone =
    note.kind === "ok"
      ? "border-[var(--color-nucleus-border)] text-[var(--color-nucleus-faint)]"
      : "border-[var(--color-status-down)] text-[var(--color-status-down)]";
  return <div className={`rounded border px-3 py-2 text-xs ${tone}`}>{note.text}</div>;
}

/** The draft as Markdown, its frontmatter as a small block above it. */
function RenderedDraft({ text }: { text: string }) {
  const { frontmatter, body } = splitFrontmatter(text);
  return (
    <div className="space-y-3">
      {frontmatter !== null && (
        <pre className="overflow-x-auto whitespace-pre-wrap rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-3 py-2 text-[11px] leading-relaxed text-[var(--color-nucleus-faint)]">
          {frontmatter}
        </pre>
      )}
      <Markdown source={body} />
    </div>
  );
}

function Evidence({ s }: { s: Suggestion }) {
  return (
    <section>
      <Label>evidence · {s.evidence.length} {s.evidence.length === 1 ? "session" : "sessions"}</Label>
      {s.evidence.length === 0 ? (
        <div className="text-xs text-[var(--color-nucleus-faint)]">none recorded</div>
      ) : (
        <ul className="space-y-3">
          {s.evidence.map((e) => (
            <li key={e.id} className="text-xs">
              <div className="flex flex-wrap items-center gap-x-2 gap-y-0.5 text-[var(--color-nucleus-faint)]">
                <code className="text-[var(--color-nucleus-text)]">{e.session_id ?? "(no session)"}</code>
                <span>{evidenceSourceLabel(e.source)}</span>
                {e.agent && <span>{e.agent}</span>}
                <span>{shortDate(e.created_at)}</span>
              </div>
              {e.note && <p className="mt-1 leading-relaxed text-[var(--color-nucleus-text)]">{e.note}</p>}
              {e.quotes.length > 0 && (
                <div className="mt-1.5 space-y-1.5">
                  {e.quotes.map((q, k) => (
                    <blockquote
                      key={k}
                      className="whitespace-pre-wrap break-words border-l-2 border-[var(--color-nucleus-border)] pl-3 leading-relaxed text-[var(--color-nucleus-faint)]"
                    >
                      {q}
                    </blockquote>
                  ))}
                </div>
              )}
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

/** The reject step: an optional reason, then reject or keep. Styled like
 *  `InlineConfirm`; Escape keeps the suggestion. */
function RejectStrip({
  id,
  reason,
  onReason,
  busy,
  onConfirm,
  onCancel,
}: {
  id: number;
  reason: string;
  onReason: (r: string) => void;
  busy: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const inputRef = useRef<HTMLInputElement>(null);
  useEffect(() => {
    inputRef.current?.focus();
  }, []);
  return (
    <div
      role="alertdialog"
      aria-label={`Reject suggestion #${id}`}
      onKeyDown={(e) => {
        if (e.key === "Escape" && !busy) {
          e.stopPropagation();
          onCancel();
        }
      }}
      className="flex flex-wrap items-center gap-2 border-t border-[var(--color-status-down)] px-4 py-2 text-xs"
    >
      <span className="text-[var(--color-status-down)]">Reject #{id}? Reason (optional, later reviews see it):</span>
      <input
        ref={inputRef}
        value={reason}
        onChange={(e) => onReason(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && !busy) onConfirm();
        }}
        disabled={busy}
        maxLength={1200}
        placeholder="for example: too narrow, already covered by another skill"
        className="min-w-[12rem] flex-1 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-2 py-0.5 text-[var(--color-nucleus-text)] placeholder:text-[var(--color-nucleus-faint)] focus:border-[var(--color-nucleus-accent)] focus:outline-none"
      />
      <button
        onClick={onConfirm}
        disabled={busy}
        className="rounded border border-[var(--color-status-down)] px-2 py-0.5 text-[var(--color-status-down)] transition-colors hover:bg-[var(--color-nucleus-bg)] disabled:opacity-40"
      >
        {busy ? "rejecting…" : "reject"}
      </button>
      <button
        onClick={onCancel}
        disabled={busy}
        className="rounded border border-[var(--color-nucleus-border)] px-2 py-0.5 text-[var(--color-nucleus-faint)] transition-colors hover:text-[var(--color-nucleus-text)] disabled:opacity-40"
      >
        keep
      </button>
    </div>
  );
}

function EditDraft({
  outcome,
  onDismiss,
  draft,
  onDraft,
  busy,
  onSave,
  onCancel,
}: {
  /** The result of the last save (a refusal lists the draft's problems). */
  outcome: Outcome | null;
  onDismiss: () => void;
  draft: string;
  onDraft: (d: string) => void;
  busy: boolean;
  onSave: () => void;
  onCancel: () => void;
}) {
  return (
    <div className="space-y-2">
      <Label>edit the draft SKILL.md</Label>
      <p className="text-[11px] leading-relaxed text-[var(--color-nucleus-faint)]">
        Saving checks the draft again (frontmatter, the required sections, the size limit, credential-shaped
        text). The name and ownership fields are set by Nucleus. A model revision never replaces an edited draft.
      </p>
      {outcome && <OutcomePanel outcome={outcome} onDismiss={onDismiss} />}
      <textarea
        value={draft}
        onChange={(e) => onDraft(e.target.value)}
        disabled={busy}
        spellCheck={false}
        rows={Math.min(32, Math.max(12, draft.split("\n").length + 1))}
        aria-label="draft SKILL.md"
        className="w-full rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-3 py-2 font-mono text-[12px] leading-relaxed text-[var(--color-nucleus-text)] focus:border-[var(--color-nucleus-accent)] focus:outline-none"
      />
      <div className="flex items-center gap-2">
        <SmallButton onClick={onSave} disabled={busy}>
          <Check size={11} strokeWidth={2} />
          {busy ? "saving…" : "save draft"}
        </SmallButton>
        <SmallButton onClick={onCancel} disabled={busy}>
          cancel
        </SmallButton>
      </div>
    </div>
  );
}

function SmallButton({
  children,
  onClick,
  disabled,
  danger = false,
}: {
  children: ReactNode;
  onClick: () => void;
  disabled?: boolean;
  danger?: boolean;
}) {
  return (
    <button
      onClick={onClick}
      disabled={disabled}
      className={[
        "flex items-center gap-1.5 rounded border border-[var(--color-nucleus-border)] px-2 py-0.5 text-[11px] text-[var(--color-nucleus-faint)] transition-colors disabled:pointer-events-none disabled:opacity-40",
        danger
          ? "enabled:hover:border-[var(--color-status-down)] enabled:hover:text-[var(--color-status-down)]"
          : "enabled:hover:border-[var(--color-nucleus-accent)] enabled:hover:text-[var(--color-nucleus-accent)]",
      ].join(" ")}
    >
      {children}
    </button>
  );
}
