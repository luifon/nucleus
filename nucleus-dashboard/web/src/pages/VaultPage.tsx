import { useMemo, useState } from "react";
import { RefreshCw, Database } from "lucide-react";
import PageShell from "@/components/PageShell";
import Select from "@/components/Select";
import VaultFileRow from "@/components/vault/VaultFileRow";
import { useFetch } from "@/lib/hooks";
import { listRecentVault, listVaultBuckets } from "@/lib/api";

const ALL = "__all__";

export default function VaultPage() {
  const [bucket, setBucket] = useState<string>(ALL);

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
          vault <span className="text-[var(--color-nucleus-faint)]">/ recent writes</span>
        </>
      }
      subtitle="Filesystem mtime feed across the Obsidian vault. No write-audit log exists today (ADR-015 §Future work), so this reflects 'what files changed recently' rather than 'what the brain-dump apply did'."
      actions={
        <button
          onClick={() => { buckets.refetch(); files.refetch(); }}
          className="flex items-center gap-1.5 rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)] px-2.5 py-1 text-xs text-[var(--color-nucleus-faint)] hover:text-[var(--color-nucleus-accent)]"
        >
          <RefreshCw size={12} strokeWidth={1.75} />
          refresh
        </button>
      }
    >
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
    </PageShell>
  );
}
