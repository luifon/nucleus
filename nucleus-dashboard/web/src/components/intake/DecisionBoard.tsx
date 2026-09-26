import { useEffect, useRef, useState } from "react";
import CanvasFrame from "@/components/chat/CanvasFrame";
import {
  answerQuestion,
  approvePlan,
  cancelItem,
  releaseItem,
  retryItem,
  type IntakeHiddenFinding,
  type IntakeItem,
  type IntakeQuestion,
  type IntakeReplyResult,
} from "@/lib/api";
import {
  boardFor,
  cancelStep,
  findingKindLabel,
  findingPlace,
  moveHighlight,
  questionStep,
  type BoardOption,
} from "@/lib/intake";

// The decision board (ADR-036): at the bottom of the conversation, in
// place of the composer, while the item waits for the operator. The
// options come from code (`boardFor`), never from model text. It works like
// Claude Code's option prompts: the arrow keys move the highlight, Enter
// selects it, a tap or a click selects an option directly. "Cancel item"
// asks a second step; approve, release and retry run at once (the option
// names the version or the hold). A confirmation question the server asked
// after text typed on the page is a Yes / No step. After an action the page
// refreshes; a refusal shows the server's text here.

/** The routes the board calls; tests pass fakes. */
export interface BoardActions {
  approve: (id: number, version: number) => Promise<IntakeItem>;
  release: (id: number, hold: string) => Promise<IntakeItem>;
  retry: (id: number) => Promise<IntakeItem>;
  cancel: (id: number) => Promise<IntakeItem>;
  answer: (id: number, question: number, yes: boolean) => Promise<IntakeReplyResult>;
}

const API_ACTIONS: BoardActions = {
  approve: approvePlan,
  release: releaseItem,
  retry: retryItem,
  cancel: cancelItem,
  answer: answerQuestion,
};

type Step = "options" | "cancel";

export default function DecisionBoard({
  item,
  question,
  onWrite,
  onChange,
  actions = API_ACTIONS,
  initialStep = "options",
  autoFocus = true,
  findings = [],
}: {
  item: IntakeItem;
  /** The page's open confirmation question: shown as a Yes / No step. */
  question: IntakeQuestion | null;
  /** The hidden-content findings of a held item: listed above the options,
   *  so the operator sees what "Release" lets through. */
  findings?: readonly IntakeHiddenFinding[];
  /** "Continue discussing" / "Write a message": show the composer. */
  onWrite: () => void;
  /** Receives the item an action returned; the page refetches. */
  onChange: (item: IntakeItem) => void;
  actions?: BoardActions;
  /** The step shown first (tests). */
  initialStep?: Step;
  /** Focus the highlighted option when the board appears. */
  autoFocus?: boolean;
}) {
  const [step, setStep] = useState<Step>(initialStep);
  const [highlight, setHighlight] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const refs = useRef<(HTMLButtonElement | null)[]>([]);
  const mounted = useRef(false);

  const board = boardFor(item);
  const view = question
    ? { kind: "confirm", key: `q${question.id}`, ...questionStep(item.id, question) }
    : step === "cancel"
      ? { kind: "confirm", key: "cancel", ...cancelStep(item.id) }
      : board.kind === "board"
        ? { kind: "decision", key: `b${board.title}`, title: board.title, options: board.options }
        : { kind: "decision", key: "none", title: "", options: [] as BoardOption[] };

  // A new step starts at its first option, with the focus on it (on the
  // first render only when `autoFocus`).
  useEffect(() => {
    setHighlight(0);
    if (mounted.current || autoFocus) refs.current[0]?.focus({ preventScroll: true });
    mounted.current = true;
  }, [view.key, autoFocus]);

  const run = async (fn: () => Promise<IntakeItem>) => {
    setBusy(true);
    setError(null);
    try {
      const next = await fn();
      setStep("options");
      onChange(next);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(false);
    }
  };

  const select = (o: BoardOption) => {
    if (busy) return;
    switch (o.key) {
      case "approve":
        return void run(() => actions.approve(item.id, item.plan_version));
      case "release":
        return void run(() => actions.release(item.id, item.hold_hash ?? ""));
      case "retry":
        return void run(() => actions.retry(item.id));
      case "cancel":
        setError(null);
        return setStep("cancel");
      case "discuss":
      case "write":
        return onWrite();
      case "yes":
      case "no":
        if (question) return void run(async () => (await actions.answer(item.id, question.id, o.key === "yes")).item);
        if (o.key === "yes") return void run(() => actions.cancel(item.id));
        setError(null);
        return setStep("options");
    }
  };

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === "Enter") {
      e.preventDefault();
      const o = view.options[highlight];
      if (o) select(o);
      return;
    }
    if (e.key === "Escape" && step === "cancel" && !question) {
      e.preventDefault();
      setStep("options");
      return;
    }
    const next = moveHighlight(highlight, e.key, view.options.length);
    if (next === null) return;
    e.preventDefault();
    setHighlight(next);
    refs.current[next]?.focus({ preventScroll: true });
  };

  return (
    <div className="shrink-0 border-t border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] p-3" data-board={view.kind}>
      <CanvasFrame kind={view.kind} title={view.title}>
        {item.stage === "held" && findings.length > 0 && (step === "options" || question?.decision === "release") && (
          <ul aria-label="hidden content" className="mb-2 max-h-40 space-y-1 overflow-y-auto">
            {findings.map((f, i) => (
              <li key={i} className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-2 py-1 text-xs">
                <span className="text-[10px] text-[var(--color-nucleus-faint)]">
                  {findingPlace(f)} · <span className="text-[var(--color-status-warn)]">{findingKindLabel(f.kind)}</span>
                </span>
                <div className="whitespace-pre-wrap break-all font-mono text-[var(--color-nucleus-text)] [font-variant-ligatures:none]">{f.text}</div>
              </li>
            ))}
          </ul>
        )}
        <div role="listbox" aria-label={view.title} aria-busy={busy} onKeyDown={onKeyDown} className="flex flex-col gap-1">
          {view.options.map((o, i) => {
            const on = i === highlight;
            const tone =
              o.tone === "down"
                ? "text-[var(--color-status-down)]"
                : o.tone === "accent"
                  ? "text-[var(--color-nucleus-accent)]"
                  : "text-[var(--color-nucleus-text)]";
            return (
              <button
                key={o.key}
                ref={(el) => {
                  refs.current[i] = el;
                }}
                type="button"
                role="option"
                aria-selected={on}
                tabIndex={on ? 0 : -1}
                data-option={o.key}
                disabled={busy}
                onClick={() => {
                  setHighlight(i);
                  select(o);
                }}
                onMouseEnter={() => setHighlight(i)}
                onFocus={() => setHighlight(i)}
                className={[
                  "flex min-h-[2.5rem] w-full items-baseline gap-2 rounded border px-2.5 py-2 text-left text-sm transition-colors focus:outline-none disabled:opacity-50",
                  on
                    ? "border-[var(--color-nucleus-accent)] bg-[color-mix(in_srgb,var(--color-nucleus-accent)_10%,transparent)]"
                    : "border-transparent hover:border-[var(--color-nucleus-border)]",
                ].join(" ")}
              >
                <span aria-hidden className="w-3 shrink-0 text-[var(--color-nucleus-accent)]">
                  {on ? "❯" : ""}
                </span>
                <span className="shrink-0 text-[var(--color-nucleus-faint)]">{i + 1}.</span>
                <span className="min-w-0 flex-1">
                  <span className={tone}>{o.label}</span>
                  {o.hint && <span className="ml-2 text-xs text-[var(--color-nucleus-faint)]">{o.hint}</span>}
                </span>
              </button>
            );
          })}
        </div>
        {error && (
          <div role="alert" className="mt-2 whitespace-pre-wrap break-words text-xs text-[var(--color-status-down)]">
            {error}
          </div>
        )}
        <div className="mt-2 hidden text-[10px] text-[var(--color-nucleus-faint)] md:block">
          {busy ? "working…" : "↑ ↓ to move · Enter to select"}
        </div>
        {busy && <div className="mt-2 text-[10px] text-[var(--color-nucleus-faint)] md:hidden">working…</div>}
      </CanvasFrame>
    </div>
  );
}
