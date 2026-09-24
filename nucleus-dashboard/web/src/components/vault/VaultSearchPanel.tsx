import { useEffect, useState } from "react";
import { ChevronDown, ChevronRight, Search } from "lucide-react";
import Select, { type SelectOption } from "@/components/Select";
import BucketBadge from "./BucketBadge";
import { type VaultSearchHit, type VaultSearchResult, getVaultFile, searchVault } from "@/lib/api";
import { splitSnippet } from "@/lib/vault";

// ADR-035 full-text search over the vault. Same index and ranking as the
// `nucleus vault-search` CLI; credential notes and excluded folders are
// never returned. The query runs 300 ms after the last keystroke.

const ALL = "__all__";

export default function VaultSearchPanel({ bucketOptions }: { bucketOptions: SelectOption<string>[] }) {
  const [query, setQuery] = useState("");
  const [bucket, setBucket] = useState<string>(ALL);
  const [result, setResult] = useState<VaultSearchResult | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);

  useEffect(() => {
    const q = query.trim();
    if (!q) {
      setResult(null);
      setError(null);
      return;
    }
    const ctrl = new AbortController();
    const timer = window.setTimeout(() => {
      setLoading(true);
      searchVault(q, { bucket: bucket === ALL ? undefined : bucket, limit: 30 }, ctrl.signal)
        .then((r) => {
          setResult(r);
          setError(null);
        })
        .catch((e) => {
          if ((e as Error)?.name !== "AbortError") setError(String(e));
        })
        .finally(() => {
          if (!ctrl.signal.aborted) setLoading(false);
        });
    }, 300);
    return () => {
      window.clearTimeout(timer);
      ctrl.abort();
    };
  }, [query, bucket]);

  return (
    <div>
      <div className="mb-5 flex flex-wrap items-center gap-4 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-4 py-2.5">
        <label className="flex min-w-[16rem] flex-1 items-center gap-2 text-sm">
          <Search size={14} strokeWidth={1.75} className="shrink-0 text-[var(--color-nucleus-faint)]" />
          <input
            type="search"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="search note titles, headings, tags and text…"
            aria-label="search the vault"
            autoFocus
            className="w-full bg-transparent text-[var(--color-nucleus-text)] outline-none placeholder:text-[var(--color-nucleus-faint)]"
          />
        </label>
        <Select label="bucket" options={bucketOptions} value={bucket} onChange={setBucket} />
        <div className="text-xs text-[var(--color-nucleus-faint)]">
          {loading ? "searching…" : result ? `${result.hits.length} ${result.hits.length === 1 ? "note" : "notes"}` : ""}
        </div>
      </div>

      {error ? (
        <div className="rounded border border-[var(--color-status-down)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-sm text-[var(--color-status-down)]">
          {error}
        </div>
      ) : !result ? (
        <div className="text-sm text-[var(--color-nucleus-faint)]">
          Words must all match; if no note has every word, notes with some of them are shown. Quotes search an exact phrase.
        </div>
      ) : result.hits.length === 0 ? (
        <div className="text-sm text-[var(--color-nucleus-faint)]">no notes match</div>
      ) : (
        <>
          {result.mode === "any" && (
            <div className="mb-3 text-xs text-[var(--color-status-warn)]">
              No note contains every word; showing notes that contain some of them.
            </div>
          )}
          <ul className="space-y-1.5">
            {result.hits.map((h) => (
              <li key={h.path}>
                <SearchHitRow hit={h} />
              </li>
            ))}
          </ul>
        </>
      )}
    </div>
  );
}

function SearchHitRow({ hit }: { hit: VaultSearchHit }) {
  const [expanded, setExpanded] = useState(false);
  const [body, setBody] = useState<string | null>(null);
  const [bodyErr, setBodyErr] = useState<string | null>(null);

  const toggle = async () => {
    const next = !expanded;
    setExpanded(next);
    if (next && body === null && !bodyErr) {
      try {
        setBody(await getVaultFile(hit.path));
      } catch (e) {
        setBodyErr(String(e));
      }
    }
  };

  return (
    <article className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)]">
      <button
        onClick={toggle}
        className="flex w-full flex-col gap-1 px-4 py-2.5 text-left transition-colors hover:bg-[var(--color-nucleus-bg)]"
      >
        <div className="flex w-full items-center gap-3">
          {expanded ? (
            <ChevronDown size={14} strokeWidth={1.75} className="shrink-0 text-[var(--color-nucleus-faint)]" />
          ) : (
            <ChevronRight size={14} strokeWidth={1.75} className="shrink-0 text-[var(--color-nucleus-faint)]" />
          )}
          <BucketBadge bucket={hit.bucket} />
          <span className="min-w-0 truncate text-sm text-[var(--color-nucleus-text)]" title={hit.path}>
            {hit.display}
          </span>
          {hit.title && hit.title !== hit.display.replace(/\.md$/, "") && (
            <span className="min-w-0 truncate text-xs text-[var(--color-nucleus-faint)]">{hit.title}</span>
          )}
          <span className="ml-auto shrink-0 text-[11px] tabular-nums text-[var(--color-nucleus-faint)]">
            {hit.created ?? ""}
          </span>
        </div>
        <div className="pl-[26px] text-xs leading-relaxed text-[var(--color-nucleus-faint)]">
          {splitSnippet(hit.snippet).map((p, i) =>
            p.match ? (
              <mark key={i} className="bg-transparent text-[var(--color-nucleus-accent)]">
                {p.text}
              </mark>
            ) : (
              <span key={i}>{p.text}</span>
            ),
          )}
        </div>
        <div className="pl-[26px] text-[11px] text-[var(--color-nucleus-faint)] opacity-70">{hit.path}</div>
      </button>
      {expanded && (
        <div className="border-t border-[var(--color-nucleus-border)] px-4 py-3">
          {bodyErr ? (
            <div className="text-xs text-[var(--color-status-down)]">{bodyErr}</div>
          ) : body === null ? (
            <div className="text-xs text-[var(--color-nucleus-faint)]">loading…</div>
          ) : (
            <pre className="max-h-96 overflow-auto whitespace-pre-wrap text-[12px] leading-relaxed text-[var(--color-nucleus-text)]">
              {body}
            </pre>
          )}
        </div>
      )}
    </article>
  );
}
