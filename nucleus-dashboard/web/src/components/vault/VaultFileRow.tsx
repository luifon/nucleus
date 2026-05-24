import { useState } from "react";
import { ChevronDown, ChevronRight, FileText, ExternalLink } from "lucide-react";
import BucketBadge from "./BucketBadge";
import { type VaultFile, getVaultFile } from "@/lib/api";

// One row per recently-touched vault file. Header: bucket badge +
// path + mtime + size. Click to expand and load the file body
// inline. Pre-formatted text for v1; could become a real markdown
// renderer if this surface becomes a primary read path.

export default function VaultFileRow({ file }: { file: VaultFile }) {
  const [expanded, setExpanded] = useState(false);
  const [body, setBody] = useState<string | null>(null);
  const [bodyErr, setBodyErr] = useState<string | null>(null);

  const toggle = async () => {
    const next = !expanded;
    setExpanded(next);
    if (next && body === null && !bodyErr) {
      try {
        setBody(await getVaultFile(file.path));
      } catch (e) {
        setBodyErr(String(e));
      }
    }
  };

  // Display the path without the leading `bucket/` segment since
  // the badge already conveys it. Root-level files keep their full
  // relpath (which is the filename itself).
  const tail = file.bucket && file.relpath.startsWith(file.bucket + "/")
    ? file.relpath.slice(file.bucket.length + 1)
    : file.relpath;

  // Build an obsidian:// deep-link so the operator can open the
  // file in Obsidian directly. Falls back to a noop if Obsidian
  // can't intercept the URL.
  const vaultDir = file.path.slice(0, file.path.length - file.relpath.length - 1);
  const vaultName = vaultDir.split("/").pop() ?? "";
  const obsidianUrl =
    vaultName && file.relpath
      ? `obsidian://open?vault=${encodeURIComponent(vaultName)}&file=${encodeURIComponent(file.relpath.replace(/\.md$/, ""))}`
      : null;

  return (
    <article className="rounded border border-[var(--color-nucleus-border)] bg-[var(--color-nucleus-surface)]">
      <button
        onClick={toggle}
        className="flex w-full items-center gap-3 px-4 py-2.5 text-left transition-colors hover:bg-[var(--color-nucleus-bg)]"
      >
        {expanded ? (
          <ChevronDown size={14} strokeWidth={1.75} className="shrink-0 text-[var(--color-nucleus-faint)]" />
        ) : (
          <ChevronRight size={14} strokeWidth={1.75} className="shrink-0 text-[var(--color-nucleus-faint)]" />
        )}
        <BucketBadge bucket={file.bucket} />
        <span className="min-w-0 flex-1 truncate text-sm text-[var(--color-nucleus-text)]" title={file.relpath}>
          {tail}
        </span>
        <span className="shrink-0 text-[11px] tabular-nums text-[var(--color-nucleus-faint)]" title={fullTime(file.mtime_unix)}>
          {relTime(file.mtime_unix)}
        </span>
        <span className="flex shrink-0 items-center gap-1 text-[11px] text-[var(--color-nucleus-faint)]">
          <FileText size={10} strokeWidth={1.75} />
          {humanBytes(file.bytes)}
        </span>
      </button>
      {expanded && (
        <div className="border-t border-[var(--color-nucleus-border)] px-4 py-3">
          {obsidianUrl && (
            <div className="mb-2">
              <a
                href={obsidianUrl}
                onClick={(e) => e.stopPropagation()}
                className="inline-flex items-center gap-1 text-[11px] text-[var(--color-nucleus-accent)] hover:text-[var(--color-nucleus-text)]"
              >
                <ExternalLink size={10} strokeWidth={1.75} />
                open in Obsidian
              </a>
            </div>
          )}
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

function relTime(unix: number): string {
  if (!unix) return "—";
  const sec = Math.floor(Date.now() / 1000) - unix;
  if (sec < 60) return `${sec}s ago`;
  if (sec < 3600) return `${Math.floor(sec / 60)}m ago`;
  if (sec < 86400) return `${Math.floor(sec / 3600)}h ago`;
  return `${Math.floor(sec / 86400)}d ago`;
}

function fullTime(unix: number): string {
  if (!unix) return "—";
  return new Date(unix * 1000).toLocaleString("en-GB");
}

function humanBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}
