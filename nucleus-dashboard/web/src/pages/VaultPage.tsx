import { useMemo, useState } from "react";
import { useNavigate, useParams } from "react-router-dom";
import { RefreshCw, Database } from "lucide-react";
import PageShell from "@/components/PageShell";
import Select from "@/components/Select";
import Tabs from "@/components/Tabs";
import VaultFileRow from "@/components/vault/VaultFileRow";
import VaultSearchPanel from "@/components/vault/VaultSearchPanel";
import VaultCheckPanel from "@/components/vault/VaultCheckPanel";
import { useFetch } from "@/lib/hooks";
import { listRecentVault, listVaultBuckets } from "@/lib/api";

const ALL = "__all__";

type TabValue = "recent" | "search" | "check";

const SUBTITLES: Record<TabValue, string> = {
  recent:
    "Filesystem mtime feed across the Obsidian vault. No write-audit log exists today (ADR-015 §Future work), so this reflects 'what files changed recently' rather than 'what the brain-dump apply did'.",
  search:
    "Full-text search over the vault (ADR-035): titles, headings, tags, frontmatter and text. Credential notes and excluded folders are never indexed or shown.",
  check:
    "Weekly structural check (ADR-035): duplicates, broken links, orphans, stale inbox, frontmatter, sources, empty files. Written by nucleus vault-check.",
};

// Tabs map to routes so the WhatsApp summary can link straight to the
// check (`/vault/check`).
function tabFromParam(p: string | undefined): TabValue {
  return p === "search" || p === "check" ? p : "recent";
}

export default function VaultPage() {
  const { tab: tabParam } = useParams();
  const navigate = useNavigate();
  const tab = tabFromParam(tabParam);
  const [bucket, setBucket] = useState<string>(ALL);
  const [checkRefresh, setCheckRefresh] = useState(0);

  const buckets = useFetch(listVaultBuckets);
  const files = useFetch(
    () => listRecentVault({ bucket: bucket === ALL ? undefined : bucket, limit: 50 }),
    [bucket],
  );

  const options = useMemo(() => {
    const base = [{ value: ALL, label: "all buckets" }];
    if (!buckets.data) return base;
    return base.concat(
      buckets.data.map((b) => ({ value: b.name, label: `${b.name} (${b.file_count})` })),
    );
  }, [buckets.data]);

  return (
    <PageShell
      title={
        <>
          vault <span className="text-[var(--color-nucleus-faint)]">/ {tab === "recent" ? "recent writes" : tab}</span>
        </>
      }
      subtitle={SUBTITLES[tab]}
      actions={
        tab !== "search" && (
          <button
            onClick={() => {
              if (tab === "check") {
                setCheckRefresh((n) => n + 1);
              } else {
                buckets.refetch();
                files.refetch();
              }
            }}
            className="flex items-center gap-1.5 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-2.5 py-1 text-xs text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
          >
            <RefreshCw size={12} strokeWidth={1.75} />
            refresh
          </button>
        )
      }
    >
      <Tabs<TabValue>
        tabs={[
          { value: "recent", label: "recent" },
          { value: "search", label: "search" },
          { value: "check", label: "check" },
        ]}
        value={tab}
        onChange={(next) => navigate(next === "recent" ? "/vault" : `/vault/${next}`)}
      />

      {tab === "search" ? (
        <VaultSearchPanel bucketOptions={options} />
      ) : tab === "check" ? (
        <VaultCheckPanel refreshKey={checkRefresh} />
      ) : (
        <>
          <div className="mb-5 flex flex-wrap items-center gap-4 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-4 py-2.5">
            <Select label="bucket" options={options} value={bucket} onChange={setBucket} />
            <div className="ml-auto text-xs text-[var(--color-nucleus-faint)]">
              {files.data
                ? `${files.data.length} ${files.data.length === 1 ? "file" : "files"}`
                : files.loading
                  ? "fetching…"
                  : (files.error ?? "")}
            </div>
          </div>

          {files.error ? (
            <div className="rounded border border-[var(--color-status-down)] bg-[var(--color-nucleus-surface)] px-3 py-2 text-sm text-[var(--color-status-down)]">
              {files.error}
            </div>
          ) : !files.data ? (
            <div className="text-sm text-[var(--color-nucleus-faint)]">fetching…</div>
          ) : files.data.length === 0 ? (
            <div className="flex items-center gap-2 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-3 py-6 text-sm text-[var(--color-nucleus-faint)]">
              <Database size={14} strokeWidth={1.75} />
              no recent writes{bucket !== ALL ? ` in ${bucket}` : ""}
            </div>
          ) : (
            <ul className="space-y-1.5">
              {files.data.map((f) => (
                <li key={f.path}>
                  <VaultFileRow file={f} />
                </li>
              ))}
            </ul>
          )}
        </>
      )}
    </PageShell>
  );
}
