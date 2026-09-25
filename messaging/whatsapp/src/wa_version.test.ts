// WA Web version cache (ADR-027 amendment, 2026-09; Rule 8).

import { test, beforeEach } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { DEFAULT_CONNECTION_CONFIG, type WAVersion } from "@whiskeysockets/baileys";
import { resetWaVersionMemory, resolveWaVersion, type FetchResult } from "./wa_version.js";

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

test("a corrupt cache file is ignored", async () => {
  const file = cachePath();
  fs.mkdirSync(path.dirname(file), { recursive: true });
  fs.writeFileSync(file, "{not json");
  const r = await resolveWaVersion({ cachePath: file, fetchLatest: failed });
  assert.equal(r.source, "bundled");
});
