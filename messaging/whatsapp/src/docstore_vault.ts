// Vault manifest mirror for the document library (ADR-018).
//
// The vault never holds document bytes — only this text layer under
// 4-Areas/Documents/:
//
//   Documents-overview.md  — area hub. Written once IF MISSING; the
//                            operator may curate it afterwards.
//   manifest.md            — a regenerated VIEW of documents.db (the DB is
//                            the single source of truth). Full rewrite on
//                            every mutation via tmp+rename: idempotent,
//                            rename/retag-safe, self-healing if corrupted.
//   audit.md               — APPEND-ONLY event log. Different semantics on
//                            purpose: appends never need correction, and
//                            the human-readable trail survives even if
//                            documents.db is lost (a real durability hedge
//                            given the conscious single-disk deferral).
//
// Direct deterministic writes (diary.ts precedent) — NOT the braindump
// review pipeline: there's nothing for a model to decide; the paths are
// fixed and the content derives from the DB. Rule 9 note: creating the
// 4-Areas/Documents/ sub-folder is operator-directed via ADR-018.

import { DatabaseSync } from "node:sqlite";
import fs from "node:fs";
import path from "node:path";
import type { Config } from "./config.js";
import type { ManifestEvent } from "./docstore.js";

function localToday(): string {
  const d = new Date();
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(
    d.getDate(),
  ).padStart(2, "0")}`;
}

function humanBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}

export function vaultDocsDir(config: Config): string {
  return path.join(config.vaultPath, "4-Areas", "Documents");
}

/** Write the area hub once. Never overwrites — the operator may curate. */
export function ensureOverview(dir: string): void {
  const p = path.join(dir, "Documents-overview.md");
  if (fs.existsSync(p)) return;
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(
    p,
    `---
created: ${localToday()}
source: whatsapp-docstore
tags: [documents, index]
---

# Documents — Area overview

Registry of the local document library (ADR-018). The binaries live at
\`memory/documents/\` in the Nucleus workspace — NEVER in this vault; this
area holds only the text layer.

- [[manifest]] — what's in the library (regenerated view of the DB)
- [[audit]] — append-only store/retrieve/rename trail

Conventions: logical names are how you ask for things ("send me my RG");
tags group them (\`identity\`, \`br\`, …). Rename/retag via
\`npm run docs --prefix messaging/whatsapp -- rename <id> --name "…"\`.
`,
  );
}

/** Regenerate manifest.md from documents.db. The DB is the source of
 *  truth; this file is a view. Atomic via tmp+rename; preserves the
 *  original `created:` frontmatter across rewrites. */
export function rewriteManifest(documentsDbPath: string, dir: string): void {
  fs.mkdirSync(dir, { recursive: true });
  const manifestPath = path.join(dir, "manifest.md");

  // Preserve created: from the existing file.
  let created = localToday();
  try {
    const existing = fs.readFileSync(manifestPath, "utf-8");
    const m = existing.match(/^created:\s*(\S+)/m);
    if (m) created = m[1];
  } catch {
    /* first write */
  }

  interface ManifestRow {
    id: string;
    logical_name: string;
    tags: string;
    filename: string;
    mimetype: string;
    bytes: number;
    added_at: string;
    last_retrieved_at: string | null;
    retrieve_count: number;
  }
  const db = new DatabaseSync(documentsDbPath);
  let rows: ManifestRow[];
  try {
    rows = db
      .prepare(
        `SELECT id, logical_name, tags, filename, mimetype, bytes, added_at,
                last_retrieved_at, retrieve_count
           FROM documents WHERE status = 'active'
          ORDER BY logical_name COLLATE NOCASE ASC`,
      )
      .all() as unknown as ManifestRow[];
  } finally {
    db.close();
  }

  const sections = rows.map((r) => {
    let tags: string[] = [];
    try {
      const parsed = JSON.parse(r.tags);
      if (Array.isArray(parsed)) tags = parsed.map(String);
    } catch {
      /* tolerate */
    }
    const retrieved =
      r.retrieve_count > 0
        ? `${r.retrieve_count}× · last ${(r.last_retrieved_at ?? "").slice(0, 10)}`
        : "never";
    return `## ${r.logical_name}
id:: ${r.id}
tags:: ${tags.join(", ") || "—"}
file:: ${r.filename} · ${r.mimetype} · ${humanBytes(r.bytes)}
added:: ${r.added_at.slice(0, 10)}
retrieved:: ${retrieved}
`;
  });

  const body = `---
created: ${created}
generated: ${new Date().toISOString()}
source: whatsapp-docstore
tags: [documents, manifest]
---

> [!warning] Generated from \`memory/documents.db\` — manual edits will be
> overwritten on the next library mutation. Rename/retag via the docs CLI.

# Document manifest

${rows.length} active document${rows.length === 1 ? "" : "s"}. See [[audit]] for the event trail.

${sections.join("\n")}`;

  const tmp = path.join(dir, "manifest.md.tmp");
  fs.writeFileSync(tmp, body);
  fs.renameSync(tmp, manifestPath);
}

/** Append one event line to audit.md (monthly headings). */
export function appendAudit(dir: string, ev: ManifestEvent): void {
  fs.mkdirSync(dir, { recursive: true });
  const p = path.join(dir, "audit.md");
  const month = ev.at.slice(0, 7);
  let prefix = "";
  if (!fs.existsSync(p)) {
    prefix = `---
created: ${localToday()}
source: whatsapp-docstore
tags: [documents, audit]
---

# Document audit trail

Append-only. One line per library event; survives a documents.db loss.

## ${month}
`;
  } else {
    const tail = fs.readFileSync(p, "utf-8");
    if (!tail.includes(`## ${month}`)) prefix = `\n## ${month}\n`;
  }
  const stamp = `${ev.at.slice(0, 10)} ${ev.at.slice(11, 16)}`;
  const detail = ev.detail ? ` — ${ev.detail}` : "";
  const line = `- ${stamp} — ${ev.action} — [[manifest#${ev.doc.logicalName}|${ev.doc.logicalName}]] — ${ev.channel}${detail}\n`;
  fs.appendFileSync(p, prefix + line);
}

/** The hook DocStore consumers wire in: overview once, audit append, and
 *  a full manifest rewrite (retrievals too — retrieve_count is displayed). */
export function makeVaultManifestHook(config: Config): (ev: ManifestEvent) => void {
  const dir = vaultDocsDir(config);
  return (ev: ManifestEvent) => {
    ensureOverview(dir);
    appendAudit(dir, ev);
    rewriteManifest(config.documentsDbPath, dir);
  };
}
