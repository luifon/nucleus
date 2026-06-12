import { useMemo, useState } from "react";
import { File, FileText, Search } from "lucide-react";
import PageShell from "@/components/PageShell";
import StatusPill, { type StatusKind } from "@/components/StatusPill";
import Tabs from "@/components/Tabs";
import { useFetch } from "@/lib/hooks";
import {
  documentFileUrl,
  humanBytes,
  listDocumentAudit,
  listDocuments,
  parseTags,
  type DocumentRow,
} from "@/lib/api/documents";

// Documents library viewer (ADR-018). Binaries live at memory/documents/
// (never the vault); this surface lists + previews them and shows the
// audit trail. Read-only by design — store/retrieve/rename happen via
// WhatsApp + the docs CLI, and viewing here doesn't count as a retrieval.

type TabValue = "library" | "audit";

const AUDIT_KIND: Record<string, StatusKind> = {
  store: "ok",
  retrieve: "warn",
  rename: "idle",
  retag: "idle",
  delete: "down",
};

function matches(d: DocumentRow, q: string): boolean {
  const needle = q.toLowerCase();
  return (
    d.logical_name.toLowerCase().includes(needle) ||
    d.filename.toLowerCase().includes(needle) ||
    parseTags(d.tags).some((t) => t.toLowerCase().includes(needle))
  );
}

function DocumentTile({ d }: { d: DocumentRow }) {
  const isImage = d.mimetype.startsWith("image/");
  const tags = parseTags(d.tags);
  const retrieved =
    d.retrieve_count > 0
      ? `sent ${d.retrieve_count}× · last ${(d.last_retrieved_at ?? "").slice(0, 10)}`
      : "never sent";
  return (
    <button
      onClick={() => window.open(documentFileUrl(d), "_blank", "noopener")}
      className="group flex flex-col overflow-hidden rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] text-left transition-colors hover:border-[var(--color-nucleus-accent)]"
    >
      <div className="flex h-36 items-center justify-center overflow-hidden bg-[var(--color-nucleus-bg)]">
        {isImage ? (
          <img
            src={documentFileUrl(d)}
            alt={d.logical_name}
            loading="lazy"
            className="h-full w-full object-cover"
          />
        ) : d.mimetype === "application/pdf" ? (
          <FileText size={40} className="text-[var(--color-nucleus-faint)]" />
        ) : (
          <File size={40} className="text-[var(--color-nucleus-faint)]" />
        )}
      </div>
      <div className="flex flex-col gap-1 p-3">
        <div className="truncate text-sm text-[var(--color-nucleus-accent)]">
          {d.logical_name}
        </div>
        {tags.length > 0 && (
          <div className="flex flex-wrap gap-1">
            {tags.map((t) => (
              <span
                key={t}
                className="rounded border border-[var(--color-nucleus-border)] px-1.5 py-0.5 text-[10px] text-[var(--color-nucleus-faint)]"
              >
                {t}
              </span>
            ))}
          </div>
        )}
        <div className="text-xs text-[var(--color-nucleus-faint)]">
          {humanBytes(d.bytes)} · added {d.added_at.slice(0, 10)} · {retrieved}
        </div>
      </div>
    </button>
  );
}

export default function DocumentsPage() {
  const docs = useFetch(listDocuments);
  const audit = useFetch(listDocumentAudit);
  const [tab, setTab] = useState<TabValue>("library");
  const [query, setQuery] = useState("");

  const filtered = useMemo(() => {
    const all = docs.data ?? [];
    const q = query.trim();
    return q ? all.filter((d) => matches(d, q)) : all;
  }, [docs.data, query]);

  return (
    <PageShell
      title="documents"
      subtitle="Local document library (ADR-018) — binaries at memory/documents/, retrieved via WhatsApp. Viewing here doesn't count as a retrieval."
    >
      <Tabs
        tabs={[
          { value: "library", label: "library", count: docs.data?.length ?? null },
          { value: "audit", label: "audit", count: audit.data?.length ?? null },
        ]}
        value={tab}
        onChange={setTab}
      />

      {tab === "library" && (
        <>
          <div className="mb-4 flex items-center gap-2 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-2">
            <Search size={14} className="shrink-0 text-[var(--color-nucleus-faint)]" />
            <input
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder="filter by name, tag, filename…"
              className="w-full bg-transparent text-sm outline-none placeholder:text-[var(--color-nucleus-faint)]"
            />
          </div>
          {docs.error && (
            <div className="text-sm text-[var(--color-status-down)]">
              [error] {docs.error}
            </div>
          )}
          {docs.loading && !docs.data && (
            <div className="text-sm text-[var(--color-nucleus-faint)]">loading…</div>
          )}
          {docs.data && filtered.length === 0 && (
            <div className="text-sm text-[var(--color-nucleus-faint)]">
              {query ? "no documents match the filter" : "library is empty — send a file to the WhatsApp DM to archive it"}
            </div>
          )}
          <div className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-4 xl:grid-cols-5">
            {filtered.map((d) => (
              <DocumentTile key={d.id} d={d} />
            ))}
          </div>
        </>
      )}

      {tab === "audit" && (
        <>
          {audit.error && (
            <div className="text-sm text-[var(--color-status-down)]">
              [error] {audit.error}
            </div>
          )}
          <div className="flex flex-col gap-1.5">
            {(audit.data ?? []).map((a, i) => (
              <div
                key={`${a.doc_id}-${a.at}-${i}`}
                className="flex flex-wrap items-baseline gap-2 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-sm"
              >
                <span className="text-xs text-[var(--color-nucleus-faint)]">
                  {a.at.slice(0, 16).replace("T", " ")}
                </span>
                <StatusPill kind={AUDIT_KIND[a.action] ?? "idle"}>{a.action}</StatusPill>
                <span className="text-[var(--color-nucleus-text)]">
                  {a.logical_name ?? a.doc_id.slice(0, 8)}
                </span>
                <span className="text-xs text-[var(--color-nucleus-faint)]">{a.channel}</span>
                {a.detail && (
                  <span className="text-xs text-[var(--color-nucleus-faint)]">— {a.detail}</span>
                )}
              </div>
            ))}
            {audit.data && audit.data.length === 0 && (
              <div className="text-sm text-[var(--color-nucleus-faint)]">no events yet</div>
            )}
          </div>
        </>
      )}
    </PageShell>
  );
}
