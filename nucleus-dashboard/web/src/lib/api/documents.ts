// Documents library viewer (ADR-018). Wire types are ts-rs-generated
// (CLAUDE.md Rule 12) — this module adds fetchers + UI helpers only.
// The dashboard is a READ-ONLY surface over the TS-owned library;
// viewing here deliberately does not count as a retrieval.

import { jsonGet } from "./client";
import type { DocumentRow } from "./generated/DocumentRow";
import type { DocumentAuditRow } from "./generated/DocumentAuditRow";

export type { DocumentRow } from "./generated/DocumentRow";
export type { DocumentAuditRow } from "./generated/DocumentAuditRow";

export const listDocuments = (signal?: AbortSignal) =>
  jsonGet<DocumentRow[]>("/documents/api/list", signal);

export const listDocumentAudit = (signal?: AbortSignal) =>
  jsonGet<DocumentAuditRow[]>("/documents/api/audit", signal);

/** Stored-name contract (ADR-018): `${id}.${ext}` — `ext` comes from the
 *  API row (the docstore is the one implementation; we just compose). */
export const documentFileUrl = (d: DocumentRow) => `/documents/files/${d.id}.${d.ext}`;

/** Tags are stored as a JSON array string; parse tolerantly. */
export function parseTags(tags: string): string[] {
  try {
    const parsed = JSON.parse(tags);
    if (Array.isArray(parsed)) return parsed.map(String);
  } catch {
    /* fall through */
  }
  return tags ? tags.split(",").map((t) => t.trim()).filter(Boolean) : [];
}

export function humanBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / (1024 * 1024)).toFixed(1)} MB`;
}
