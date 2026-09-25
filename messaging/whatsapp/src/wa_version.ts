// WhatsApp Web protocol version for every Baileys connection (Rule 8,
// ADR-027 amendment 2026-09).
//
// `fetchLatestWaWebVersion` reads the current version from web.whatsapp.com.
// When that request fails it does not throw: it returns the version bundled
// with the Baileys release and an `error`. The bundled version goes stale,
// and every 405 login failure recorded in connection_events happened on it.
// So a successful fetch is remembered in memory and on disk
// (memory/whatsapp-wa-version.json, with its fetch time), and a failed fetch
// reuses that version while it is younger than `maxAgeMs` (default 7 days,
// `[whatsapp.link] wa_version_max_age_hours`).
//
// A 405 close means the server refused the version. `invalidateWaVersion`
// then deletes the cached version (memory and disk) and marks it rejected, so
// the next connection fetches again and, if the fetch fails, uses the
// bundled version instead of offering the refused one again. The bundled
// version is used only when there is no usable cached version.

import fs from "node:fs";
import path from "node:path";
import { DEFAULT_CONNECTION_CONFIG, fetchLatestWaWebVersion, type WAVersion } from "@whiskeysockets/baileys";

export type VersionSource = "fetched" | "memory" | "disk" | "bundled";

export const WA_VERSION_MAX_AGE_MS = 7 * 24 * 60 * 60 * 1000;

export interface FetchResult {
  version: WAVersion;
  isLatest: boolean;
  error?: unknown;
}

export interface VersionDeps {
  cachePath: string;
  fetchLatest?: () => Promise<FetchResult>;
  log?: { warn(obj: object, msg: string): void };
  now?: () => Date;
  /** A cached version older than this is not used. */
  maxAgeMs?: number;
}

interface Cached {
  version: WAVersion;
  fetchedAtMs: number;
}

/** The cache file for a workspace. */
export function waVersionCachePath(workspaceRoot: string): string {
  return path.join(workspaceRoot, "memory", "whatsapp-wa-version.json");
}

let remembered: Cached | null = null;
/** Versions the server refused (405) in this process. */
const rejected = new Set<string>();

const keyOf = (v: WAVersion) => v.join(".");

/** Forget the in-memory state (tests). */
export function resetWaVersionMemory(): void {
  remembered = null;
  rejected.clear();
}

function isVersion(v: unknown): v is WAVersion {
  return Array.isArray(v) && v.length === 3 && v.every((n) => Number.isInteger(n) && n >= 0);
}

function readDisk(file: string): Cached | null {
  try {
    const parsed = JSON.parse(fs.readFileSync(file, "utf8")) as { version?: unknown; fetchedAt?: unknown };
    const at = typeof parsed.fetchedAt === "string" ? Date.parse(parsed.fetchedAt) : NaN;
    // An entry without a valid fetch time has no known age: not used.
    if (!isVersion(parsed.version) || !Number.isFinite(at)) return null;
    return { version: parsed.version, fetchedAtMs: at };
  } catch {
    return null;
  }
}

function writeDisk(file: string, c: Cached): void {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, `${JSON.stringify({ version: c.version, fetchedAt: new Date(c.fetchedAtMs).toISOString() })}\n`);
  fs.renameSync(tmp, file);
}

/** The server refused `version` (a 405 close). Delete the cached version if
 *  it is that version, and never offer it again from the cache in this
 *  process. A fetch that returns it again is still used: the fetch is the
 *  authority on the current version. */
export function invalidateWaVersion(cachePath: string, version: WAVersion): void {
  rejected.add(keyOf(version));
  if (remembered && keyOf(remembered.version) === keyOf(version)) remembered = null;
  const disk = readDisk(cachePath);
  if (disk && keyOf(disk.version) === keyOf(version)) {
    try {
      fs.rmSync(cachePath, { force: true });
    } catch {
      /* the rejected set still keeps it out */
    }
  }
}

/** The version to connect with, and where it came from. */
export async function resolveWaVersion(d: VersionDeps): Promise<{ version: WAVersion; source: VersionSource; error?: string }> {
  const fetchLatest = d.fetchLatest ?? (() => fetchLatestWaWebVersion({}) as Promise<FetchResult>);
  const nowMs = (d.now ?? (() => new Date()))().getTime();
  const maxAgeMs = d.maxAgeMs ?? WA_VERSION_MAX_AGE_MS;
  let res: FetchResult | null = null;
  let error: string | undefined;
  try {
    res = await fetchLatest();
    if (res.error || !res.isLatest || !isVersion(res.version)) {
      const e = res.error as { message?: unknown } | undefined;
      error = typeof e?.message === "string" ? e.message : String(res.error ?? "not the latest version");
      res = null;
    }
  } catch (e) {
    error = (e as Error)?.message ?? String(e);
    res = null;
  }
  if (res) {
    const fresh = { version: res.version, fetchedAtMs: nowMs };
    remembered = fresh;
    rejected.delete(keyOf(res.version));
    try {
      writeDisk(d.cachePath, fresh);
    } catch (e) {
      d.log?.warn({ err: (e as Error).message }, "whatsapp: could not write the WA Web version cache");
    }
    return { version: res.version, source: "fetched" };
  }
  const usable = (c: Cached | null): c is Cached =>
    c !== null && nowMs - c.fetchedAtMs <= maxAgeMs && !rejected.has(keyOf(c.version));
  if (usable(remembered)) return { version: remembered.version, source: "memory", error };
  const disk = readDisk(d.cachePath);
  if (usable(disk)) {
    remembered = disk;
    return { version: disk.version, source: "disk", error };
  }
  // No usable cached version: the library's bundled version is all there is.
  d.log?.warn({ err: error }, "whatsapp: WA Web version fetch failed and no usable cached version exists — using the bundled version");
  return { version: DEFAULT_CONNECTION_CONFIG.version, source: "bundled", error };
}
