// Outbound media helpers (ADR-018) — pure functions the drain composes.
//
// Lifecycle contract (the part that must never regress): a media row's
// `media_path` is a DRAIN-OWNED staged file under memory/outbound-staging/.
// It is unlinked ONLY at terminal state — markSent, or a markFailure that
// RETURNS status 'failed', or markFailedTerminal. A retried (still-pending)
// row's file must survive. Filesystem actions live in the drain, never in
// the store.

import fs from "node:fs";
import path from "node:path";
import type { AnyMessageContent } from "@whiskeysockets/baileys";
import pino from "pino";
import type { OutboundKind, OutboundRow } from "./db.js";

const log = pino({ level: process.env.NUCLEUS_LOG ?? "info" });

export const SEND_TIMEOUT_TEXT_MS = 20_000;
/** Media uploads stream multi-MB files over a residential uplink. */
export const SEND_TIMEOUT_MEDIA_MS = 90_000;
/** Cap media sends per tick so one batch can't monopolize the drain. */
export const MAX_MEDIA_SENDS_PER_TICK = 3;
/** Default size cap; override via WHATSAPP_MEDIA_MAX_BYTES. WhatsApp allows
 *  ~2GB documents, but 64MB is the honest budget for a 90s timeout. Images
 *  effectively cap ~16MB on WhatsApp's side — send bigger ones as documents. */
export const DEFAULT_MEDIA_MAX_BYTES = 64 * 1024 * 1024;

/** Worst legitimate tick, DERIVED so the bound can't drift from the
 *  constants it protects (pre-ADR-018 this was a hand-computed literal):
 *  (20 - 3) text sends at 20s + 3 media sends at 90s + 60s margin. */
export const DRAIN_WATCHDOG_MS =
  (20 - MAX_MEDIA_SENDS_PER_TICK) * SEND_TIMEOUT_TEXT_MS +
  MAX_MEDIA_SENDS_PER_TICK * SEND_TIMEOUT_MEDIA_MS +
  60_000;

export function sendTimeoutFor(kind: OutboundKind): number {
  return kind === "text" ? SEND_TIMEOUT_TEXT_MS : SEND_TIMEOUT_MEDIA_MS;
}

/** Build the Baileys message content for a queue row. Uses the {url} stream
 *  form for media — Baileys streams the file during upload instead of
 *  holding MBs in heap, and the staged file persists across retries by
 *  design. Returns {error} for rows that can never succeed (missing or
 *  oversized file) — the caller marks those terminal. */
export function buildOutboundContent(
  r: OutboundRow,
  mediaMaxBytes: number = DEFAULT_MEDIA_MAX_BYTES,
): AnyMessageContent | { error: string } {
  if (r.kind === "text") {
    return { text: r.body };
  }
  if (!r.mediaPath) {
    return { error: `media row #${r.id} has no media_path` };
  }
  let size: number;
  try {
    size = fs.statSync(r.mediaPath).size;
  } catch {
    return { error: `media file missing: ${r.mediaPath}` };
  }
  if (size > mediaMaxBytes) {
    return { error: `media file too large: ${size} bytes > ${mediaMaxBytes} cap` };
  }
  const caption = r.body.trim() ? r.body : undefined;
  if (r.kind === "image") {
    return {
      image: { url: r.mediaPath },
      caption,
      mimetype: r.mimetype ?? undefined,
    };
  }
  return {
    document: { url: r.mediaPath },
    mimetype: r.mimetype ?? "application/octet-stream",
    fileName: r.filename ?? path.basename(r.mediaPath),
    caption,
  };
}

/** Unlink a media row's staged file. Tolerates ENOENT (already gone). */
export function cleanupMedia(r: OutboundRow): void {
  if (!r.mediaPath) return;
  try {
    fs.unlinkSync(r.mediaPath);
    log.info({ id: r.id, path: r.mediaPath }, "whatsapp: staged media cleaned up");
  } catch (e) {
    if ((e as NodeJS.ErrnoException).code !== "ENOENT") {
      log.warn(
        { id: r.id, path: r.mediaPath, err: (e as Error).message },
        "whatsapp: staged media cleanup failed",
      );
    }
  }
}

/** Boot sweep: delete files in the staging dir not referenced by any
 *  pending row. Covers: crash between markSent and unlink; terminal rows
 *  whose unlink failed; enqueue-media crash between copy and INSERT.
 *  Only ever touches the staging dir. Returns the count swept. */
export function sweepOutboundStaging(
  stagingDir: string,
  pendingPaths: string[],
): number {
  let entries: string[];
  try {
    entries = fs.readdirSync(stagingDir);
  } catch {
    return 0; // dir doesn't exist yet — nothing staged ever
  }
  const keep = new Set(pendingPaths.map((p) => path.resolve(p)));
  let swept = 0;
  for (const name of entries) {
    const full = path.resolve(stagingDir, name);
    if (keep.has(full)) continue;
    try {
      fs.unlinkSync(full);
      swept += 1;
    } catch (e) {
      log.warn(
        { path: full, err: (e as Error).message },
        "whatsapp: staging sweep failed for file",
      );
    }
  }
  if (swept > 0) {
    log.info({ swept, stagingDir }, "whatsapp: outbound staging swept");
  }
  return swept;
}
