import { useEffect, useRef, useState } from "react";
import { useSearchParams } from "react-router-dom";
import { ChevronDown, ChevronRight, Lightbulb } from "lucide-react";
import SectionHeader from "@/components/SectionHeader";
import OutcomePanel, { type Outcome } from "@/components/skills/OutcomePanel";
import SuggestionRow from "@/components/skills/SuggestionRow";
import { useFetch } from "@/lib/hooks";
import { getSuggestions } from "@/lib/api";
import { splitSuggestions, suggestionFromSearch, withSuggestion } from "@/lib/suggestions";

// Skill suggestions (ADR-038) at the top of the Skills page: the pending
// ones first, then the decided ones (accepted, rejected, superseded) in a
// collapsed history. One suggestion is open at a time; the open one is in
// the URL (`/skills?suggestion=<id>`), which is also the link the WhatsApp
// notice sends. An accept or a reject closes the suggestion (it moves to the
// history) and shows its result at the top of the section.

export default function SuggestionsSection({ onLibraryChanged }: { onLibraryChanged: () => void }) {
  const list = useFetch(getSuggestions);
  const [params, setParams] = useSearchParams();
  const open = suggestionFromSearch(params);
  const [showHistory, setShowHistory] = useState(false);
  const [decided, setDecided] = useState<Outcome | null>(null);
  const top = useRef<HTMLElement>(null);

  const { pending, history } = splitSuggestions(list.data ?? []);
  const openInHistory = open !== null && history.some((s) => s.id === open);
  const openUnknown = open !== null && list.data !== null && !list.data.some((s) => s.id === open);

  // A deep link to a decided suggestion opens the history.
  useEffect(() => {
    if (openInHistory) setShowHistory(true);
  }, [openInHistory]);

  const setOpen = (id: number | null) => setParams(withSuggestion(params, id), { replace: true });
  const toggle = (id: number) => setOpen(open === id ? null : id);
  const onDecided = (o: Outcome) => {
    setDecided(o);
    setOpen(null);
    top.current?.scrollIntoView({ block: "start" });
  };

  const rows = (items: typeof pending) => (
    <ul className="space-y-2">
      {items.map((s) => (
        <li key={s.id}>
          <SuggestionRow
            summary={s}
            open={open === s.id}
            onToggle={() => toggle(s.id)}
            onOpen={setOpen}
            onChanged={list.refetch}
            onLibraryChanged={onLibraryChanged}
            onDecided={onDecided}
          />
        </li>
      ))}
    </ul>
  );

  return (
    <section ref={top} aria-label="skill suggestions" className="mb-8 scroll-mt-4">
      <SectionHeader label={`suggestions · ${pending.length} pending`} />
      <p className="mb-4 text-xs leading-relaxed text-[var(--color-nucleus-faint)]">
        The skill-gap learner reviews session transcripts and proposes new private skills and patches
        of existing ones. Nothing is written to the library until you accept a suggestion; a rejection
        and its reason are shown to later reviews.
      </p>

      {decided && <OutcomePanel outcome={decided} onDismiss={() => setDecided(null)} />}

      {list.error ? (
        <div className="rounded border border-[var(--color-status-down)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-sm text-[var(--color-status-down)]">
          {list.error}
        </div>
      ) : !list.data ? (
        <div className="text-sm text-[var(--color-nucleus-faint)]">fetching…</div>
      ) : (
        <>
          {openUnknown && (
            <div className="mb-3 text-xs text-[var(--color-status-down)]">There is no suggestion #{open}.</div>
          )}
          {pending.length === 0 ? (
            <div className="flex items-center gap-3 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-4 py-4 text-sm text-[var(--color-nucleus-faint)]">
              <Lightbulb size={14} strokeWidth={1.75} className="text-[var(--color-nucleus-accent)]" />
              No pending suggestions.
            </div>
          ) : (
            rows(pending)
          )}
          {history.length > 0 && (
            <div className="mt-3">
              <button
                onClick={() => setShowHistory((v) => !v)}
                aria-expanded={showHistory}
                className="flex items-center gap-1.5 text-xs text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
              >
                {showHistory ? <ChevronDown size={12} strokeWidth={1.75} /> : <ChevronRight size={12} strokeWidth={1.75} />}
                history ({history.length}): accepted, rejected and superseded
              </button>
              {showHistory && <div className="mt-2">{rows(history)}</div>}
            </div>
          )}
        </>
      )}
    </section>
  );
}
