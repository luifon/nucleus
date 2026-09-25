// WhatsApp Web protocol version for every Baileys connection (Rule 8,
// ADR-027 amendment 2026-09).
//
// `fetchLatestWaWebVersion` reads the current version from web.whatsapp.com.
// When that request fails it does not throw: it returns the version bundled
// with the Baileys release and an `error`. The bundled version goes stale,
// and every 405 login failure recorded in connection_events happened on it.
// So a successful fetch is remembered in memory and on disk
// (memory/whatsapp-wa-version.json), and a failed fetch reuses the last
// fetched version. The bundled version is used only when no version was ever
// fetched on this machine.

import fs from "node:fs";
import path from "node:path";
import { DEFAULT_CONNECTION_CONFIG, fetchLatestWaWebVersion, type WAVersion } from "@whiskeysockets/baileys";

export type VersionSource = "fetched" | "memory" | "disk" | "bundled";

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
}

/** The cache file for a workspace. */
export function waVersionCachePath(workspaceRoot: string): string {
  return path.join(workspaceRoot, "memory", "whatsapp-wa-version.json");
}

let remembered: WAVersion | null = null;

/** Forget the in-memory version (tests). */
export function resetWaVersionMemory(): void {
  remembered = null;
}

function isVersion(v: unknown): v is WAVersion {
  return Array.isArray(v) && v.length === 3 && v.every((n) => Number.isInteger(n) && n >= 0);
}

function readDisk(file: string): WAVersion | null {
  try {
    const parsed = JSON.parse(fs.readFileSync(file, "utf8")) as { version?: unknown };
    return isVersion(parsed.version) ? parsed.version : null;
  } catch {
    return null;
  }
}

function writeDisk(file: string, version: WAVersion, at: Date): void {
  fs.mkdirSync(path.dirname(file), { recursive: true });
  const tmp = `${file}.${process.pid}.tmp`;
  fs.writeFileSync(tmp, `${JSON.stringify({ version, fetchedAt: at.toISOString() })}\n`);
  fs.renameSync(tmp, file);
}

/** The version to connect with, and where it came from. */
export async function resolveWaVersion(d: VersionDeps): Promise<{ version: WAVersion; source: VersionSource; error?: string }> {
  const fetchLatest = d.fetchLatest ?? (() => fetchLatestWaWebVersion({}) as Promise<FetchResult>);
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
    remembered = res.version;
    try {
      writeDisk(d.cachePath, res.version, (d.now ?? (() => new Date()))());
    } catch (e) {
      d.log?.warn({ err: (e as Error).message }, "whatsapp: could not write the WA Web version cache");
    }
    return { version: res.version, source: "fetched" };
  }
  if (remembered) return { version: remembered, source: "memory", error };
  const disk = readDisk(d.cachePath);
  if (disk) {
    remembered = disk;
    return { version: disk, source: "disk", error };
  }
  // Nothing fetched ever: the library's bundled version is all there is.
  d.log?.warn({ err: error }, "whatsapp: WA Web version fetch failed and no cached version exists — using the bundled version");
  return { version: DEFAULT_CONNECTION_CONFIG.version, source: "bundled", error };
}
