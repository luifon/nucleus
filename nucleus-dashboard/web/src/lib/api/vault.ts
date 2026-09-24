// Vault API — chronological feed of Obsidian vault writes via
// filesystem mtime. Mirrors nucleus-dashboard/api/src/handlers/vault.rs.
// Per ADR-015 §"Scope" there's no audit log for brain-dump
// applies, so this surface answers "what files changed recently"
// rather than "what the apply pipeline did" — close enough for the
// operator's day-to-day "what did the bot write?" question.
// ADR-035 adds full-text search and the weekly vault check report.
// Wire types are ts-rs-generated from the Rust structs (./generated/).

import { jsonGet, qs } from "./client";
import type { Bucket as VaultBucket } from "./generated/Bucket";
import type { VaultFile } from "./generated/VaultFile";
import type { VaultSearchResult } from "./generated/VaultSearchResult";
import type { CheckReport } from "./generated/CheckReport";
import type { CheckRunSummary } from "./generated/CheckRunSummary";

export type { Bucket as VaultBucket } from "./generated/Bucket";
export type { VaultFile } from "./generated/VaultFile";
export type { VaultSearchHit } from "./generated/VaultSearchHit";
export type { VaultSearchResult } from "./generated/VaultSearchResult";
export type { CheckReport } from "./generated/CheckReport";
export type { CheckRunSummary } from "./generated/CheckRunSummary";
export type { CheckCounts } from "./generated/CheckCounts";
export type { Finding as VaultFinding } from "./generated/Finding";

export const listVaultBuckets = () => jsonGet<VaultBucket[]>("/vault/api/buckets");

export const listRecentVault = (opts: { bucket?: string; limit?: number } = {}) =>
  jsonGet<VaultFile[]>(`/vault/api/recent${qs({ bucket: opts.bucket, limit: opts.limit })}`);

/** `path` is absolute (recent feed) or vault-relative (search hits). */
export const getVaultFile = (path: string) =>
  fetch(`/vault/api/file${qs({ path })}`).then(async (r) => {
    if (!r.ok) throw new Error(`/vault/api/file → ${r.status}`);
    return r.text();
  });

export const searchVault = (
  q: string,
  opts: { bucket?: string; limit?: number } = {},
  signal?: AbortSignal,
) =>
  jsonGet<VaultSearchResult>(
    `/vault/api/search${qs({ q, bucket: opts.bucket, limit: opts.limit })}`,
    signal,
  );

/** Latest vault-check report; `null` before the first run. */
export const getLatestVaultCheck = (signal?: AbortSignal) =>
  jsonGet<CheckReport | null>("/vault/api/check/latest", signal);

/** Recorded runs, newest first. */
export const listVaultCheckRuns = (limit = 26, signal?: AbortSignal) =>
  jsonGet<CheckRunSummary[]>(`/vault/api/check/runs${qs({ limit })}`, signal);
