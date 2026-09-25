// WA Web version cache (ADR-027 amendment, 2026-09; Rule 8).

import { test, beforeEach } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { DEFAULT_CONNECTION_CONFIG, type WAVersion } from "@whiskeysockets/baileys";
import {
  WA_VERSION_MAX_AGE_MS,
  invalidateWaVersion,
  resetWaVersionMemory,
  resolveWaVersion,
  type FetchResult,
} from "./wa_version.js";

const FETCHED: WAVersion = [2, 3000, 1099999999];
const ok = async (): Promise<FetchResult> => ({ version: FETCHED, isLatest: true });
// What fetchLatestWaWebVersion returns on a failed request: the bundled version and an error.
const failed = async (): Promise<FetchResult> => ({
  version: DEFAULT_CONNECTION_CONFIG.version,
  isLatest: false,
  error: { message: "fetch failed" },
});
const throws = async (): Promise<FetchResult> => {
  throw new Error("network down");
};

function cachePath(): string {
  return path.join(fs.mkdtempSync(path.join(os.tmpdir(), "nucleus-waver-")), "memory", "whatsapp-wa-version.json");
}

beforeEach(() => resetWaVersionMemory());

test("a fetched version is used and written to disk", async () => {
  const file = cachePath();
  const r = await resolveWaVersion({ cachePath: file, fetchLatest: ok });
  assert.deepEqual(r, { version: FETCHED, source: "fetched" });
  assert.deepEqual(JSON.parse(fs.readFileSync(file, "utf8")).version, FETCHED);
});

test("a failed fetch reuses the version remembered in memory, not the bundled one", async () => {
  const file = cachePath();
  await resolveWaVersion({ cachePath: file, fetchLatest: ok });
  fs.rmSync(file);
  const r = await resolveWaVersion({ cachePath: file, fetchLatest: failed });
  assert.deepEqual(r.version, FETCHED);
  assert.equal(r.source, "memory");
  assert.equal(r.error, "fetch failed");
});

test("after a restart a failed fetch reuses the version on disk", async () => {
  const file = cachePath();
  await resolveWaVersion({ cachePath: file, fetchLatest: ok });
  resetWaVersionMemory(); // a new process
  for (const fetchLatest of [failed, throws]) {
    resetWaVersionMemory();
    const r = await resolveWaVersion({ cachePath: file, fetchLatest });
    assert.deepEqual(r.version, FETCHED);
    assert.equal(r.source, "disk");
  }
});

test("with no version ever fetched, the bundled version is used with a warning", async () => {
  const warnings: string[] = [];
  const r = await resolveWaVersion({ cachePath: cachePath(), fetchLatest: throws, log: { warn: (_o, m) => warnings.push(m) } });
  assert.equal(r.source, "bundled");
  assert.deepEqual(r.version, DEFAULT_CONNECTION_CONFIG.version);
  assert.equal(warnings.length, 1);
});

test("a cached version older than the age limit is not used (memory and disk)", async () => {
  const file = cachePath();
  const t0 = new Date("2026-09-01T00:00:00Z");
  await resolveWaVersion({ cachePath: file, fetchLatest: ok, now: () => t0 });
  const later = new Date(t0.getTime() + WA_VERSION_MAX_AGE_MS + 60_000);
  const fromMemory = await resolveWaVersion({ cachePath: file, fetchLatest: failed, now: () => later });
  assert.equal(fromMemory.source, "bundled", "the remembered version expired");
  resetWaVersionMemory();
  const fromDisk = await resolveWaVersion({ cachePath: file, fetchLatest: failed, now: () => later });
  assert.equal(fromDisk.source, "bundled", "the disk entry expired");
  // Inside the limit it is still used.
  resetWaVersionMemory();
  const inside = new Date(t0.getTime() + WA_VERSION_MAX_AGE_MS - 60_000);
  assert.equal((await resolveWaVersion({ cachePath: file, fetchLatest: failed, now: () => inside })).source, "disk");
});

test("a disk entry without a fetch time is not used", async () => {
  const file = cachePath();
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, JSON.stringify({ version: FETCHED }));
  assert.equal((await resolveWaVersion({ cachePath: file, fetchLatest: failed })).source, "bundled");
});

test("405 → invalidate → the next connection fetches again, and falls back to the bundled version, not the refused one", async () => {
  const file = cachePath();
  await resolveWaVersion({ cachePath: file, fetchLatest: ok });
  // The server refuses the cached version (close 405).
  invalidateWaVersion(file, FETCHED);
  assert.equal(fs.existsSync(file), false, "the disk entry is deleted");
  let fetched = 0;
  const failing = async () => {
    fetched += 1;
    return failed();
  };
  const r = await resolveWaVersion({ cachePath: file, fetchLatest: failing });
  assert.equal(fetched, 1, "a fresh fetch was tried");
  assert.equal(r.source, "bundled");
  assert.notDeepEqual(r.version, FETCHED);
  // A later successful fetch is used even if it is the same version.
  const again = await resolveWaVersion({ cachePath: file, fetchLatest: ok });
  assert.equal(again.source, "fetched");
});

test("invalidating another version keeps a newer cached one", async () => {
  const file = cachePath();
  await resolveWaVersion({ cachePath: file, fetchLatest: ok });
  invalidateWaVersion(file, [2, 3000, 1]);
  const r = await resolveWaVersion({ cachePath: file, fetchLatest: failed });
  assert.equal(r.source, "memory");
  assert.ok(fs.existsSync(file));
});

test("a corrupt cache file is ignored", async () => {
  const file = cachePath();
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, "{not json");
  const r = await resolveWaVersion({ cachePath: file, fetchLatest: failed });
  assert.equal(r.source, "bundled");
});
