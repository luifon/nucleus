import { useEffect, useRef, useState, type MouseEvent, type ReactNode } from "react";
import { ArrowUp, ArrowDown, ExternalLink } from "lucide-react";
import { type NewsItem } from "@/lib/api";
import { VOTE_REASONS, clampNote, reasonChipLabel } from "@/lib/news";

export type NewsCardVariant = "hero" | "notable" | "rest";

export type NewsCardActions = {
  onVote: (vote: 1 | -1 | 0) => void;
  /** A reason picked from the strip; `note` only with `other`. Resolves to
   *  whether it was stored. */
  onReason: (key: string, note?: string) => Promise<boolean>;
  /** The reason strip was asked for (a downvote, or a click on the reason chip). */
  onReasonOpen: () => void;
  /** The strip was left without picking anything. */
  onReasonClose: () => void;
  /** A link of the item was opened. */
  onOpen: (url: string) => void;
};

// Shared news-item card. Hero is full-width with amber border + full summary.
// Notable is grid-cell sized, summary clamped. Rest is compact, no summary.
//
// While `reasonOpen` is set, the card's meta line is replaced by the widget's
// reason chips (ADR-031). A downvoted title fades but keeps its place, and an
// opened item carries a dot, as on the widget.
export default function NewsCard({
  item,
  variant,
  reasonOpen,
  sharedEvent,
  actions,
}: {
  item: NewsItem;
  variant: NewsCardVariant;
  reasonOpen: boolean;
  /** Another listed item has the same event; show the event label. */
  sharedEvent: boolean;
  actions: NewsCardActions;
}) {
  const score = item.notable_score ?? 0;
  const titleInk = item.vote === -1 ? "text-[var(--color-nucleus-text)]/55" : "text-[var(--color-nucleus-text)]";
  const title = (size: string) => (
    <span className="flex items-baseline gap-2">
      {item.opened && (
        <span
          aria-label="opened"
          title="opened"
          className="relative -top-0.5 inline-block h-1.5 w-1.5 shrink-0 rounded-full bg-[var(--color-nucleus-faint)]"
        />
      )}
      <ItemLink
        href={item.url}
        onOpen={actions.onOpen}
        className={`block ${size} leading-snug ${titleInk} hover:text-[var(--color-nucleus-accent)]`}
      >
        {item.title}
      </ItemLink>
    </span>
  );
  const meta = (children: ReactNode) =>
    reasonOpen ? <ReasonStrip onPick={actions.onReason} onClose={actions.onReasonClose} /> : children;

  if (variant === "hero") {
    return (
      <article className="rounded border-2 border-[var(--color-nucleus-accent)] bg-[var(--color-nucleus-surface)] p-5">
        <div className="mb-3 flex items-center gap-2 text-[11px] uppercase tracking-widest text-[var(--color-nucleus-accent)]">
          <span>◆ top story · score {score.toFixed(2)}</span>
        </div>
        <div className="flex items-start gap-4">
          <div className="min-w-0 flex-1">
            {title("text-xl")}
            {item.summary && (
              <p className="mt-3 line-clamp-6 break-words text-sm leading-relaxed text-[var(--color-nucleus-faint)]">
                {item.summary}
              </p>
            )}
            <div className="mt-4 flex flex-wrap items-center gap-x-4 gap-y-1 text-[12px]">
              {meta(
                <>
                  <span className="text-[var(--color-nucleus-faint)]">pub {item.published_date}</span>
                  <span className="text-[var(--color-status-ok)]">{item.source_name}</span>
                  <ReasonChip item={item} onOpen={actions.onReasonOpen} />
                  {sharedEvent && <EventLabel slug={item.event_slug} />}
                  <ArticleLink item={item} onOpen={actions.onOpen} size={11} />
                  {item.notable_reason && (
                    <span className="italic text-[#7dd9cc]">{item.notable_reason}</span>
                  )}
                </>,
              )}
            </div>
          </div>
          <VoteButtons vote={item.vote} onVote={actions.onVote} stacked />
        </div>
      </article>
    );
  }

  if (variant === "notable") {
    return (
      <article className="rounded border border-[var(--color-nucleus-accent)] bg-[var(--color-nucleus-surface)] p-3.5">
        <div className="mb-1.5 text-[10px] uppercase tracking-widest text-[var(--color-nucleus-accent)]">
          notable
        </div>
        {title("text-base")}
        {item.summary && (
          <p className="mt-2 line-clamp-3 break-words text-[12px] leading-relaxed text-[var(--color-nucleus-faint)]">
            {item.summary}
          </p>
        )}
        {item.notable_reason && (
          <p className="mt-1.5 line-clamp-2 text-[11px] italic text-[#7dd9cc]">
            {item.notable_reason}
          </p>
        )}
        <CardFooter item={item} sharedEvent={sharedEvent} actions={actions} meta={meta} />
      </article>
    );
  }

  // rest
  return (
    <article className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] p-3">
      {title("text-[14px]")}
      <CardFooter item={item} sharedEvent={sharedEvent} actions={actions} meta={meta} compact />
    </article>
  );
}

function CardFooter({
  item,
  sharedEvent,
  actions,
  meta,
  compact,
}: {
  item: NewsItem;
  sharedEvent: boolean;
  actions: NewsCardActions;
  meta: (children: ReactNode) => ReactNode;
  compact?: boolean;
}) {
  const score = item.notable_score ?? 0;
  return (
    <div className={`${compact ? "mt-2" : "mt-3"} flex items-center gap-2 text-[11px] text-[var(--color-nucleus-faint)]`}>
      <div className="flex min-w-0 flex-1 flex-wrap items-center gap-x-2 gap-y-1">
        {meta(
          <>
            <span>pub {item.published_date}</span>
            <span className="text-[var(--color-status-ok)]">{item.source_name}</span>
            {score > 0 && (
              <span
                className={
                  score >= 0.7
                    ? "text-[var(--color-status-ok)]"
                    : score >= 0.4
                      ? "text-[var(--color-status-warn)]"
                      : "text-[var(--color-nucleus-faint)]"
                }
                title={`notable_score = ${score.toFixed(3)}`}
              >
                {score.toFixed(2)}
              </span>
            )}
            <ReasonChip item={item} onOpen={actions.onReasonOpen} />
            {sharedEvent && <EventLabel slug={item.event_slug} />}
            <ArticleLink item={item} onOpen={actions.onOpen} size={10} />
          </>,
        )}
      </div>
      <span className="shrink-0 self-start">
        <VoteButtons vote={item.vote} onVote={actions.onVote} />
      </span>
    </div>
  );
}

// A link to the item that records the open. Middle-click opens a tab too, so
// it counts; a context-menu "open in new tab" cannot be observed.
function ItemLink({
  href,
  onOpen,
  className,
  children,
}: {
  href: string;
  onOpen: (url: string) => void;
  className: string;
  children: ReactNode;
}) {
  return (
    <a
      href={href}
      target="_blank"
      rel="noreferrer"
      onClick={() => onOpen(href)}
      onAuxClick={(e: MouseEvent) => {
        if (e.button === 1) onOpen(href);
      }}
      className={className}
    >
      {children}
    </a>
  );
}

function ArticleLink({ item, onOpen, size }: { item: NewsItem; onOpen: (url: string) => void; size: number }) {
  if (!item.article_url || item.article_url === item.url) return null;
  return (
    <ItemLink
      href={item.article_url}
      onOpen={onOpen}
      className="flex items-center gap-1 text-[var(--color-nucleus-accent)] hover:text-[var(--color-nucleus-text)]"
    >
      <ExternalLink size={size} strokeWidth={1.75} /> article
    </ItemLink>
  );
}

function EventLabel({ slug }: { slug: string | null }) {
  if (!slug) return null;
  return (
    <span className="text-[var(--color-nucleus-faint)]" title="other listed items report the same event">
      event {slug}
    </span>
  );
}

// The reader's reason, or on a downvote without one, an offer to give it.
// Either one reopens the strip.
function ReasonChip({ item, onOpen }: { item: NewsItem; onOpen: () => void }) {
  if (item.vote !== -1) return null;
  const reason = item.vote_reason;
  const base = "rounded border px-1.5 py-px text-[10px] leading-tight";
  return (
    <button
      type="button"
      onClick={(e) => {
        e.preventDefault();
        onOpen();
      }}
      title={reason ? (item.vote_note ?? "change the reason") : "say why you downvoted this"}
      className={
        reason
          ? `${base} border-[var(--color-status-down)]/45 text-[var(--color-status-down)] hover:border-[var(--color-status-down)]`
          : `${base} border-[var(--color-nucleus-border)] text-[var(--color-nucleus-faint)] hover:border-[var(--color-nucleus-text)] hover:text-[var(--color-nucleus-text)]`
      }
    >
      {reason ? reasonChipLabel(reason) : "why?"}
    </button>
  );
}

// The widget's chip strip. `other…` opens a note field; Enter saves a
// non-empty note, Escape leaves. A press outside the strip, or Escape, closes
// it without recording anything: the downvote stands without a reason. The
// strip stays open until a reason is stored, so a failed save keeps the note.
//
// Focus moves to the first choice when the strip opens, and back to the
// card's downvote button when Escape closes it: the control that opened the
// strip is replaced by it.
function ReasonStrip({
  onPick,
  onClose,
}: {
  onPick: (key: string, note?: string) => Promise<boolean>;
  onClose: () => void;
}) {
  const ref = useRef<HTMLDivElement>(null);
  const [writing, setWriting] = useState(false);
  const [note, setNote] = useState("");
  const [pending, setPending] = useState(false);

  useEffect(() => {
    ref.current?.querySelector<HTMLButtonElement>("button")?.focus();
  }, []);

  useEffect(() => {
    const onPointer = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      ref.current?.closest("article")?.querySelector<HTMLButtonElement>("[data-vote=down]")?.focus();
      onClose();
    };
    document.addEventListener("pointerdown", onPointer);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onPointer);
      document.removeEventListener("keydown", onKey);
    };
  }, [onClose]);

  const pick = (key: string, text?: string) => {
    if (pending) return;
    setPending(true);
    void onPick(key, text).then((ok) => {
      if (!ok) setPending(false);
    });
  };

  const chip =
    "rounded border border-[var(--color-nucleus-border)] px-1.5 py-px text-[10px] leading-tight text-[var(--color-nucleus-faint)] hover:border-[var(--color-nucleus-text)] hover:text-[var(--color-nucleus-text)] disabled:opacity-50";

  return (
    <div ref={ref} className="flex w-full flex-wrap items-center gap-1.5" role="group" aria-label="downvote reason">
      {writing ? (
        <>
          <input
            autoFocus
            value={note}
            disabled={pending}
            aria-label="why you downvoted this"
            placeholder="why?"
            onChange={(e) => setNote(clampNote(e.target.value))}
            onKeyDown={(e) => {
              if (e.key === "Enter") {
                e.preventDefault();
                const text = note.trim();
                if (text) pick("other", text);
              }
            }}
            className="min-w-0 flex-1 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-bg)] px-1.5 py-0.5 text-[11px] text-[var(--color-nucleus-text)] outline-none focus:border-[var(--color-nucleus-accent)]"
          />
          <span className="text-[10px] text-[var(--color-nucleus-faint)]">↵ save · esc cancel</span>
        </>
      ) : (
        VOTE_REASONS.map((r) => (
          <button
            key={r.key}
            type="button"
            className={chip}
            disabled={pending}
            onClick={(e) => {
              e.preventDefault();
              if (r.key === "other") setWriting(true);
              else pick(r.key);
            }}
          >
            {r.label}
          </button>
        ))
      )}
    </div>
  );
}

// A vote is a state, not a tally (ADR-031): one reader, latest verdict wins.
// The buttons show which way the item is currently voted, not how many times.
function VoteButtons({
  vote,
  onVote,
  stacked,
}: {
  vote: number;
  onVote: (vote: 1 | -1 | 0) => void;
  stacked?: boolean;
}) {
  const base =
    "flex items-center rounded border px-1.5 py-0.5 text-[11px] border-[var(--color-nucleus-border)] text-[var(--color-nucleus-faint)]";
  return (
    <div className={`flex ${stacked ? "flex-col" : "flex-row"} items-center gap-1`}>
      <button
        onClick={(e) => { e.preventDefault(); onVote(vote === 1 ? 0 : 1); }}
        title={vote === 1 ? "upvoted — click to clear" : "upvote"}
        aria-pressed={vote === 1}
        className={
          vote === 1
            ? `${base} border-[var(--color-status-ok)] text-[var(--color-status-ok)]`
            : `${base} hover:border-[var(--color-status-ok)] hover:text-[var(--color-status-ok)]`
        }
      >
        <ArrowUp size={11} strokeWidth={2} />
      </button>
      <button
        data-vote="down"
        onClick={(e) => { e.preventDefault(); onVote(vote === -1 ? 0 : -1); }}
        title={vote === -1 ? "downvoted — click to clear" : "downvote"}
        aria-pressed={vote === -1}
        className={
          vote === -1
            ? `${base} border-[var(--color-status-down)] text-[var(--color-status-down)]`
            : `${base} hover:border-[var(--color-status-down)] hover:text-[var(--color-status-down)]`
        }
      >
        <ArrowDown size={11} strokeWidth={2} />
      </button>
    </div>
  );
}
