// Document library store (ADR-018) — local binaries + sqlite metadata.
//
// OWNERSHIP (ADR-020): memory/documents.db is written ONLY by the whatsapp
// package family — the bot process and its CLIs (docs.ts, enqueue-media.ts).
// Same-family concurrency is absorbed by WAL + busy_timeout. Everything
// else (notably the Rust dashboard) opens READ-ONLY.
//
// Stored-name contract (normative, pinned in ADR-018): the on-disk file for
// a document is `${id}.${ext}` where ext = lower-cased extension derived
// from the original filename (fallback via mimetype map, then "bin").
// `storedName()` below is the ONE implementation; the dashboard and any
// other reader derive the same name — don't fork the logic.
//
// By-reference rule: this module moves bytes between disk locations; it
// never hands file contents to a model. Sessions get metadata + paths.

import { DatabaseSync } from "node:sqlite";
import { addColumnsIfMissing } from "./db.js";
import { createHash, randomUUID } from "node:crypto";
import fs from "node:fs";
import path from "node:path";

export type DocAction =
  | "store"
  | "retrieve"
  | "rename"
  | "retag"
  | "delete"
  | "enrich"
  | "import";

export interface DocRecord {
  id: string;
  logicalName: string;
  tags: string[];
  filename: string;
  ext: string;
  mimetype: string;
  bytes: number;
  sha256: string;
  source: string;
  addedAt: string;
  lastRetrievedAt: string | null;
  retrieveCount: number;
  /** ADR-013 enrichment: auto-generated, search-only — operator-owned
   *  `tags` always outrank these in find(). */
  keywords: string[];
  summary: string | null;
  enrichedAt: string | null;
  /** null = not attempted | ok | unsupported | failed */
  enrichStatus: string | null;
  /** Vault-relative path of the imported 5-Resources note, when imported. */
  importedPath: string | null;
}

export interface ManifestEvent {
  action: DocAction;
  doc: DocRecord;
  channel: string;
  detail?: string;
  at: string;
}

const MIME_EXT: Record<string, string> = {
  "image/jpeg": "jpg",
  "image/png": "png",
  "image/webp": "webp",
  "image/gif": "gif",
  "application/pdf": "pdf",
  "text/plain": "txt",
  "application/zip": "zip",
  "application/vnd.openxmlformats-officedocument.wordprocessingml.document": "docx",
  "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet": "xlsx",
};

/** Sanitized lower-case extension: original filename first, mimetype map
 *  second, "bin" last. Part of the stored-name contract. */
export function extFor(filename: string, mimetype: string): string {
  const fromName = path.extname(filename).replace(".", "").toLowerCase();
  if (fromName && /^[a-z0-9]{1,8}$/.test(fromName)) return fromName;
  return MIME_EXT[mimetype.toLowerCase()] ?? "bin";
}

/** The normative on-disk name for a document (ADR-018 contract). */
export function storedName(id: string, ext: string): string {
  return `${id}.${ext}`;
}

interface DocRow {
  id: string;
  logical_name: string;
  tags: string;
  filename: string;
  ext: string;
  mimetype: string;
  bytes: number;
  sha256: string;
  source: string;
  added_at: string;
  last_retrieved_at: string | null;
  retrieve_count: number;
  keywords: string;
  summary: string | null;
  enriched_at: string | null;
  enrich_status: string | null;
  imported_path: string | null;
}

function toRecord(r: DocRow): DocRecord {
  return {
    id: r.id,
    logicalName: r.logical_name,
    tags: parseJsonArray(r.tags),
    filename: r.filename,
    ext: r.ext,
    mimetype: r.mimetype,
    bytes: r.bytes,
    sha256: r.sha256,
    source: r.source,
    addedAt: r.added_at,
    lastRetrievedAt: r.last_retrieved_at,
    retrieveCount: r.retrieve_count,
    keywords: parseJsonArray(r.keywords),
    summary: r.summary,
    enrichedAt: r.enriched_at,
    enrichStatus: r.enrich_status,
    importedPath: r.imported_path,
  };
}

function parseJsonArray(raw: string | null | undefined): string[] {
  if (!raw) return [];
  try {
    const parsed = JSON.parse(raw);
    if (Array.isArray(parsed)) return parsed.map(String);
  } catch {
    /* tolerate junk */
  }
  return [];
}

export class DocStore {
  private db: DatabaseSync;
  private documentsDir: string;
  private onManifestChange?: (ev: ManifestEvent) => void;

  constructor(opts: {
    dbPath: string;
    documentsDir: string;
    /** Vault-manifest seam — fired best-effort after every mutation and
     *  retrieval; a throwing hook never breaks the operation. */
    onManifestChange?: (ev: ManifestEvent) => void;
  }) {
    fs.mkdirSync(path.dirname(opts.dbPath), { recursive: true });
    fs.mkdirSync(opts.documentsDir, { recursive: true });
    this.documentsDir = opts.documentsDir;
    this.onManifestChange = opts.onManifestChange;
    this.db = new DatabaseSync(opts.dbPath);
    this.db.exec(`PRAGMA journal_mode = WAL;`);
    this.db.exec(`PRAGMA busy_timeout = 5000;`);
    this.db.exec(`
      CREATE TABLE IF NOT EXISTS documents (
        id TEXT PRIMARY KEY,
        logical_name TEXT NOT NULL,
        tags TEXT NOT NULL DEFAULT '[]',
        filename TEXT NOT NULL,
        ext TEXT NOT NULL,
        mimetype TEXT NOT NULL,
        bytes INTEGER NOT NULL,
        sha256 TEXT NOT NULL,
        source TEXT NOT NULL,
        added_at TEXT NOT NULL,
        last_retrieved_at TEXT,
        retrieve_count INTEGER NOT NULL DEFAULT 0,
        status TEXT NOT NULL DEFAULT 'active'
      );
      CREATE INDEX IF NOT EXISTS idx_documents_name ON documents(logical_name);
      CREATE INDEX IF NOT EXISTS idx_documents_sha ON documents(sha256);

      CREATE TABLE IF NOT EXISTS doc_audit (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        doc_id TEXT NOT NULL,
        action TEXT NOT NULL,
        channel TEXT NOT NULL,
        detail TEXT,
        at TEXT NOT NULL
      );
      CREATE INDEX IF NOT EXISTS idx_doc_audit_doc ON doc_audit(doc_id, at);
    `);
    // ADR-013 enrichment columns — heal pre-enrichment DBs (S18 pattern).
    addColumnsIfMissing(this.db, "documents", [
      ["keywords", "keywords TEXT NOT NULL DEFAULT '[]'"],
      ["summary", "summary TEXT"],
      ["enriched_at", "enriched_at TEXT"],
      ["enrich_status", "enrich_status TEXT"],
      ["imported_path", "imported_path TEXT"],
    ]);
    // Boot hygiene: clear .tmp-* leftovers from a crash mid-add. ONLY
    // .tmp-* — never unknown real files (safety over tidiness).
    for (const name of fs.readdirSync(this.documentsDir)) {
      if (name.startsWith(".tmp-")) {
        try {
          fs.unlinkSync(path.join(this.documentsDir, name));
        } catch {
          /* best-effort */
        }
      }
    }
  }

  pathFor(record: Pick<DocRecord, "id" | "ext">): string {
    return path.join(this.documentsDir, storedName(record.id, record.ext));
  }

  /** Store a document. Write ordering guarantees no DB row ever points at
   *  a missing file: bytes → .tmp → fsync → rename to final name → DB
   *  insert (+audit) → manifest hook. Dedup: an active row with the same
   *  sha256 short-circuits (audited, no second copy). */
  add(input: {
    data: Buffer | { path: string };
    logicalName: string;
    tags?: string[];
    filename: string;
    mimetype: string;
    source: string;
    channel: string;
  }): { record: DocRecord; deduped: boolean } {
    const buffer = Buffer.isBuffer(input.data)
      ? input.data
      : fs.readFileSync(input.data.path);
    const sha256 = createHash("sha256").update(buffer).digest("hex");

    const existing = this.db
      .prepare(`SELECT * FROM documents WHERE sha256 = ? AND status = 'active'`)
      .get(sha256) as DocRow | undefined;
    if (existing) {
      const record = toRecord(existing);
      this.audit(record.id, "store", input.channel, `dedup of ${record.id}`);
      this.fireManifest({
        action: "store",
        doc: record,
        channel: input.channel,
        detail: "dedup",
        at: new Date().toISOString(),
      });
      return { record, deduped: true };
    }

    const id = randomUUID();
    const ext = extFor(input.filename, input.mimetype);
    const finalPath = path.join(this.documentsDir, storedName(id, ext));
    const tmpPath = path.join(this.documentsDir, `.tmp-${id}`);
    const fd = fs.openSync(tmpPath, "w");
    try {
      fs.writeSync(fd, buffer);
      fs.fsyncSync(fd);
    } finally {
      fs.closeSync(fd);
    }
    fs.renameSync(tmpPath, finalPath);

    const now = new Date().toISOString();
    try {
      this.db.exec("BEGIN IMMEDIATE");
      this.db
        .prepare(
          `INSERT INTO documents
             (id, logical_name, tags, filename, ext, mimetype, bytes, sha256,
              source, added_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
        )
        .run(
          id,
          input.logicalName,
          JSON.stringify(input.tags ?? []),
          input.filename,
          ext,
          input.mimetype,
          buffer.length,
          sha256,
          input.source,
          now,
        );
      this.db
        .prepare(
          `INSERT INTO doc_audit (doc_id, action, channel, at) VALUES (?, 'store', ?, ?)`,
        )
        .run(id, input.channel, now);
      this.db.exec("COMMIT");
    } catch (e) {
      try {
        this.db.exec("ROLLBACK");
      } catch {
        /* not in tx */
      }
      try {
        fs.unlinkSync(finalPath);
      } catch {
        /* best-effort */
      }
      throw e;
    }

    const record = this.get(id)!;
    this.fireManifest({ action: "store", doc: record, channel: input.channel, at: now });
    return { record, deduped: false };
  }

  /** Lookup by full uuid or an unambiguous prefix (≥ 4 chars). */
  get(idOrPrefix: string): DocRecord | null {
    const exact = this.db
      .prepare(`SELECT * FROM documents WHERE id = ? AND status = 'active'`)
      .get(idOrPrefix) as DocRow | undefined;
    if (exact) return toRecord(exact);
    if (idOrPrefix.length < 4) return null;
    const rows = this.db
      .prepare(`SELECT * FROM documents WHERE id LIKE ? AND status = 'active' LIMIT 2`)
      .all(`${idOrPrefix}%`) as unknown as DocRow[];
    return rows.length === 1 ? toRecord(rows[0]) : null;
  }

  /** Exact-first tiers: (1) case-insensitive exact logical_name; (2) exact
   *  tag; (3) substring on name/tags/filename; (4) token-overlap fuzzy.
   *  Each tier consulted only if the previous returned nothing. TS-side
   *  over a full active SELECT — personal-library scale (hundreds). */
  find(query: string, limit = 10): DocRecord[] {
    const q = query.trim().toLowerCase();
    if (!q) return [];
    const all = (
      this.db
        .prepare(`SELECT * FROM documents WHERE status = 'active' ORDER BY added_at DESC`)
        .all() as unknown as DocRow[]
    ).map(toRecord);

    // Tier order (ADR-013): operator-owned fields outrank auto-enrichment.
    const exact = all.filter((d) => d.logicalName.toLowerCase() === q);
    if (exact.length) return exact.slice(0, limit);

    const tagExact = all.filter((d) => d.tags.some((t) => t.toLowerCase() === q));
    if (tagExact.length) return tagExact.slice(0, limit);

    const keywordExact = all.filter((d) => d.keywords.some((k) => k.toLowerCase() === q));
    if (keywordExact.length) return keywordExact.slice(0, limit);

    const substr = all.filter(
      (d) =>
        d.logicalName.toLowerCase().includes(q) ||
        d.filename.toLowerCase().includes(q) ||
        d.tags.some((t) => t.toLowerCase().includes(q)),
    );
    if (substr.length) return substr.slice(0, limit);

    const autoSubstr = all.filter(
      (d) =>
        d.keywords.some((k) => k.toLowerCase().includes(q)) ||
        (d.summary ?? "").toLowerCase().includes(q),
    );
    if (autoSubstr.length) return autoSubstr.slice(0, limit);

    const qTokens = q.split(/\s+/).filter(Boolean);
    const scored = all
      .map((d) => {
        // Summary deliberately excluded from fuzzy: long prose makes
        // single-token overlap match everything.
        const hay = `${d.logicalName} ${d.tags.join(" ")} ${d.keywords.join(" ")} ${d.filename}`.toLowerCase();
        const score = qTokens.filter((t) => hay.includes(t)).length;
        return { d, score };
      })
      .filter((s) => s.score > 0)
      .sort((a, b) => b.score - a.score || b.d.addedAt.localeCompare(a.d.addedAt));
    return scored.slice(0, limit).map((s) => s.d);
  }

  list(opts: { tag?: string; limit?: number } = {}): DocRecord[] {
    const all = (
      this.db
        .prepare(`SELECT * FROM documents WHERE status = 'active' ORDER BY added_at DESC`)
        .all() as unknown as DocRow[]
    ).map(toRecord);
    const filtered = opts.tag
      ? all.filter((d) => d.tags.some((t) => t.toLowerCase() === opts.tag!.toLowerCase()))
      : all;
    return filtered.slice(0, opts.limit ?? 100);
  }

  /** Bump retrieval stats + audit. Called by the delivery path ONLY —
   *  dashboard views deliberately don't count (ADR-018). */
  recordRetrieval(id: string, channel: string): void {
    const now = new Date().toISOString();
    this.db
      .prepare(
        `UPDATE documents
            SET retrieve_count = retrieve_count + 1, last_retrieved_at = ?
          WHERE id = ? AND status = 'active'`,
      )
      .run(now, id);
    this.audit(id, "retrieve", channel);
    const record = this.get(id);
    if (record) {
      this.fireManifest({ action: "retrieve", doc: record, channel, at: now });
    }
  }

  /** ADR-013: store enrichment output (auto keywords + summary). Audited;
   *  manifest regenerates so the vault view shows the new fields. */
  setEnrichment(
    id: string,
    e: {
      keywords: string[];
      summary: string | null;
      status: "ok" | "unsupported" | "failed";
    },
    channel: string,
  ): void {
    const now = new Date().toISOString();
    this.db
      .prepare(
        `UPDATE documents
            SET keywords = ?, summary = ?, enriched_at = ?, enrich_status = ?
          WHERE id = ? AND status = 'active'`,
      )
      .run(JSON.stringify(e.keywords), e.summary, now, e.status, id);
    this.audit(
      id,
      "enrich",
      channel,
      e.status === "ok" ? `${e.keywords.length} keywords` : e.status,
    );
    const record = this.get(id);
    if (record) {
      this.fireManifest({ action: "enrich", doc: record, channel, at: now });
    }
  }

  /** ADR-013: record a vault import (full extracted markdown written by
   *  the import job's TS writer; this stores the pointer + audit). */
  recordImport(id: string, vaultRelPath: string, channel: string): void {
    const now = new Date().toISOString();
    this.db
      .prepare(`UPDATE documents SET imported_path = ? WHERE id = ? AND status = 'active'`)
      .run(vaultRelPath, id);
    this.audit(id, "import", channel, vaultRelPath);
    const record = this.get(id);
    if (record) {
      this.fireManifest({ action: "import", doc: record, channel, detail: vaultRelPath, at: now });
    }
  }

  rename(id: string, newName: string, channel: string): void {
    const before = this.get(id);
    if (!before) throw new Error(`no active document ${id}`);
    this.db
      .prepare(`UPDATE documents SET logical_name = ? WHERE id = ?`)
      .run(newName, id);
    this.audit(id, "rename", channel, `${before.logicalName} → ${newName}`);
    const record = this.get(id)!;
    this.fireManifest({
      action: "rename",
      doc: record,
      channel,
      detail: `${before.logicalName} → ${newName}`,
      at: new Date().toISOString(),
    });
  }

  retag(id: string, tags: string[], channel: string): void {
    const before = this.get(id);
    if (!before) throw new Error(`no active document ${id}`);
    this.db
      .prepare(`UPDATE documents SET tags = ? WHERE id = ?`)
      .run(JSON.stringify(tags), id);
    this.audit(id, "retag", channel, tags.join(","));
    const record = this.get(id)!;
    this.fireManifest({
      action: "retag",
      doc: record,
      channel,
      at: new Date().toISOString(),
    });
  }

  /** Soft delete: status='deleted' + unlink the binary. The row stays for
   *  audit history; sha-dedup ignores deleted rows (re-adding works). */
  remove(id: string, channel: string): void {
    const record = this.get(id);
    if (!record) throw new Error(`no active document ${id}`);
    this.db.prepare(`UPDATE documents SET status = 'deleted' WHERE id = ?`).run(id);
    this.audit(id, "delete", channel);
    try {
      fs.unlinkSync(this.pathFor(record));
    } catch {
      /* already gone */
    }
    this.fireManifest({
      action: "delete",
      doc: record,
      channel,
      at: new Date().toISOString(),
    });
  }

  auditRows(limit = 200): Array<{
    docId: string;
    action: string;
    channel: string;
    detail: string | null;
    at: string;
  }> {
    const rows = this.db
      .prepare(
        `SELECT doc_id, action, channel, detail, at
           FROM doc_audit ORDER BY at DESC, id DESC LIMIT ?`,
      )
      .all(limit) as Array<{
        doc_id: string;
        action: string;
        channel: string;
        detail: string | null;
        at: string;
      }>;
    return rows.map((r) => ({
      docId: r.doc_id,
      action: r.action,
      channel: r.channel,
      detail: r.detail,
      at: r.at,
    }));
  }

  private audit(docId: string, action: DocAction, channel: string, detail?: string): void {
    this.db
      .prepare(
        `INSERT INTO doc_audit (doc_id, action, channel, detail, at)
         VALUES (?, ?, ?, ?, ?)`,
      )
      .run(docId, action, channel, detail ?? null, new Date().toISOString());
  }

  private fireManifest(ev: ManifestEvent): void {
    if (!this.onManifestChange) return;
    try {
      this.onManifestChange(ev);
    } catch (e) {
      // The vault mirror is best-effort; the library op already succeeded.
      console.error(`docstore: manifest hook failed: ${(e as Error).message}`);
    }
  }
}
