import { ArrowUp, ArrowDown, ExternalLink } from "lucide-react";
import { type NewsItem } from "@/lib/api";

export type NewsCardVariant = "hero" | "notable" | "rest";

// Shared news-item card. Hero is full-width with amber border + full summary.
// Notable is grid-cell sized, summary clamped. Rest is compact, no summary.
export default function NewsCard({
  item,
  variant,
  onVote,
}: {
  item: NewsItem;
  variant: NewsCardVariant;
  onVote: (vote: 1 | -1 | 0) => void;
}) {
  const score = item.notable_score ?? 0;

  if (variant === "hero") {
    return (
      <article className="rounded border-2 border-[var(--color-nucleus-accent)] bg-[var(--color-nucleus-surface)] p-5">
        <div className="mb-3 flex items-center gap-2 text-[11px] uppercase tracking-widest text-[var(--color-nucleus-accent)]">
          <span>◆ top story · score {score.toFixed(2)}</span>
        </div>
        <div className="flex items-start gap-4">
          <div className="min-w-0 flex-1">
            <a
              href={item.url}
              target="_blank"
              rel="noreferrer"
              className="block text-xl leading-snug text-[var(--color-nucleus-text)] hover:text-[var(--color-nucleus-accent)]"
            >
              {item.title}
            </a>
            {item.summary && (
              <p className="mt-3 line-clamp-6 break-words text-sm leading-relaxed text-[var(--color-nucleus-faint)]">
                {item.summary}
              </p>
            )}
            <div className="mt-4 flex flex-wrap items-center gap-x-4 gap-y-1 text-[12px]">
              <span className="text-[var(--color-nucleus-faint)]">
                pub {item.published_date}
              </span>
              <span className="text-[var(--color-status-ok)]">{item.source_name}</span>
              {item.article_url && item.article_url !== item.url && (
                <a
                  href={item.article_url}
                  target="_blank"
                  rel="noreferrer"
                  className="flex items-center gap-1 text-[var(--color-nucleus-accent)] hover:text-[var(--color-nucleus-text)]"
                >
                  <ExternalLink size={11} strokeWidth={1.75} /> article
                </a>
              )}
              {item.notable_reason && (
                <span className="italic text-[#7dd9cc]">
                  {item.notable_reason}
                </span>
              )}
            </div>
          </div>
          <VoteButtons vote={item.vote} onVote={onVote} stacked />
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
        <a
          href={item.url}
          target="_blank"
          rel="noreferrer"
          className="block text-base leading-snug text-[var(--color-nucleus-text)] hover:text-[var(--color-nucleus-accent)]"
        >
          {item.title}
        </a>
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
        <CardFooter item={item} onVote={onVote} />
      </article>
    );
  }

  // rest
  return (
    <article className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] p-3">
      <a
        href={item.url}
        target="_blank"
        rel="noreferrer"
        className="block text-[14px] leading-snug text-[var(--color-nucleus-text)] hover:text-[var(--color-nucleus-accent)]"
      >
        {item.title}
      </a>
      <CardFooter item={item} onVote={onVote} compact />
    </article>
  );
}

function CardFooter({
  item,
  onVote,
  compact,
}: {
  item: NewsItem;
  onVote: (vote: 1 | -1 | 0) => void;
  compact?: boolean;
}) {
  const score = item.notable_score ?? 0;
  return (
    <div className={`mt-${compact ? "2" : "3"} flex items-center gap-2 text-[11px] text-[var(--color-nucleus-faint)]`}>
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
      {item.article_url && item.article_url !== item.url && (
        <a
          href={item.article_url}
          target="_blank"
          rel="noreferrer"
          className="flex items-center gap-1 text-[var(--color-nucleus-accent)] hover:text-[var(--color-nucleus-text)]"
        >
          <ExternalLink size={10} strokeWidth={1.75} /> article
        </a>
      )}
      <span className="ml-auto">
        <VoteButtons vote={item.vote} onVote={onVote} />
      </span>
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
