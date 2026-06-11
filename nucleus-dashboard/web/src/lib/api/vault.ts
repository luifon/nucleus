// Vault API — chronological feed of Obsidian vault writes via
// filesystem mtime. Mirrors nucleus-dashboard/api/src/handlers/vault.rs.
// Per ADR-015 §"Future work" there's no audit log for brain-dump
// applies, so this surface answers "what files changed recently"
// rather than "what the apply pipeline did" — close enough for the
// operator's day-to-day "what did the bot write?" question.
// Wire types are ts-rs-generated from the Rust structs (./generated/).

import { jsonGet, qs } from "./client";
import type { Bucket as VaultBucket } from "./generated/Bucket";
import type { VaultFile } from "./generated/VaultFile";

export type { Bucket as VaultBucket } from "./generated/Bucket";
export type { VaultFile } from "./generated/VaultFile";

export const listVaultBuckets = () => jsonGet<VaultBucket[]>("/vault/api/buckets");

export const listRecentVault = (opts: { bucket?: string; limit?: number } = {}) =>
  jsonGet<VaultFile[]>(`/vault/api/recent${qs({ bucket: opts.bucket, limit: opts.limit })}`);

export const getVaultFile = (path: string) =>
  fetch(`/vault/api/file${qs({ path })}`).then(async (r) => {
    if (!r.ok) throw new Error(`/vault/api/file → ${r.status}`);
    return r.text();
  });
