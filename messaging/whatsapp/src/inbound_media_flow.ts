// Inbound media (ADR-018, ADR-013): archive every image/document to the
// local library (dedup absorbs re-sends), reply with the stored name, and —
// DM role only — run an act or vault-import job the caption asks for.
// Braindump role is capture-only: archive + reply, captions become names,
// never a session ask.
//
// Every reply goes through the outbound queue (ADR-033): retried,
// allowlist-checked, secret-filtered and sent on the live socket. The quick
// in-window reply and the deferred result are two queue rows to the same
// chat.
//
// Idempotency (ADR-033). The bot records an inbound message as handled only
// after this flow returns, so a message whose handling was cut short (the
// bot stopped) is handled again when WhatsApp delivers it again. Handling it
// again creates nothing new:
//   - every reply carries the dedup key `<msgid>:<purpose>`; the queue
//     ignores a second row with the same key;
//   - the archive is deduplicated by content (docstore);
//   - every job carries the source key `<msgid>:<kind>`; a job that exists
//     is not started again. Its result is queued again under the result
//     key when the job finished (dropped if it was queued), its failure
//     likewise; a job the restart orphaned was already announced by the
//     boot sweep.

import type { WAMessage } from "@whiskeysockets/baileys";
import { classifyCaption } from "./caption.js";
import { importResultLine } from "./doc_jobs.js";
import type { OutboundQueueStore } from "./db.js";
import type { DocRecord, DocStore } from "./docstore.js";
import type { JobOutcome, JobRow, JobStore } from "./jobs.js";
import { fill, type BotTexts } from "./texts.js";

export type MediaRole = "whatsapp-group" | "braindump" | "dm";

export interface MediaDeps {
  mediaMaxBytes: number;
  docStore: Pick<DocStore, "add" | "pathFor" | "get">;
  jobStore: Pick<JobStore, "bySourceKey" | "markPromoted">;
  outbound: Pick<OutboundQueueStore, "enqueue">;
  texts: BotTexts;
  /** Persona formatting of an outbound body. */
  formatReply(body: string): string;
  download(msg: WAMessage): Promise<Buffer>;
  presence(chatId: string, state: "composing" | "paused"): Promise<void>;
  /** Fire-and-forget enrichment job (one per source key). */
  fireEnrich(record: DocRecord, chatId: string, sourceKey: string | null): void;
  /** Start an act job (one per source key). */
  startAct(o: {
    record: DocRecord;
    chatId: string;
    instruction: string;
    prompt: string;
    sourceKey: string | null;
  }): { jobId: string; promise: Promise<JobOutcome> };
  /** Run a vault import (one job per source key); resolves to the reply line. */
  runImport(record: DocRecord, chatId: string, sourceKey: string | null): Promise<string>;
  quickWindow<T>(p: Promise<T>): Promise<{ settled: true; value: T } | { settled: false }>;
  log: {
    info(o: object, m: string): void;
    warn(o: object, m: string): void;
    error(o: object, m: string): void;
  };
}

export async function handleInboundMedia(d: MediaDeps, msg: WAMessage, chatId: string, role: MediaRole): Promise<void> {
  const m = msg.message;
  const docMsg = m?.documentMessage ?? m?.documentWithCaptionMessage?.message?.documentMessage;
  const imgMsg = m?.imageMessage;
  const media = docMsg ?? imgMsg;
  if (!media) return;
  const msgId = msg.key?.id ?? null;
  const key = (purpose: string) => (msgId ? `${msgId}:${purpose}` : null);

  // The exact chat the file came from; the drain re-checks it against the
  // allowlist (an @lid DM stays @lid).
  const reply = (body: string, source: string, purpose: string) =>
    d.outbound.enqueue({ target: chatId, source, body: d.formatReply(body), dedupKey: key(purpose) });

  const isImage = !docMsg;
  const caption = (docMsg?.caption ?? imgMsg?.caption ?? "").trim();
  const origName = docMsg?.fileName ?? null;
  const mimetype = media.mimetype ?? (isImage ? "image/jpeg" : "application/octet-stream");

  // Size pre-check before downloading — fileLength is advisory but honest.
  const declared = Number(media.fileLength ?? 0);
  if (declared > d.mediaMaxBytes) {
    reply(
      fill(d.texts.fileTooLarge, {
        mb: Math.round(declared / 1024 / 1024),
        cap: Math.round(d.mediaMaxBytes / 1024 / 1024),
      }),
      "media-note",
      "too-large",
    );
    return;
  }

  let buffer: Buffer;
  try {
    buffer = await d.download(msg);
  } catch (e) {
    d.log.error({ chatId, err: (e as Error).message }, "whatsapp: media download failed");
    reply(fill(d.texts.downloadFailed, { error: (e as Error).message }), "media-note", "download-failed");
    return;
  }

  const decision = classifyCaption(caption);
  const localToday = new Date().toISOString().slice(0, 10);
  const logicalName =
    decision.name ?? (origName ? origName.replace(/\.[a-z0-9]{1,8}$/i, "") : `unnamed-${localToday}`);
  const filename = origName ?? `${logicalName}.${isImage ? "jpg" : "bin"}`;

  let record: DocRecord;
  let deduped = false;
  try {
    const res = d.docStore.add({
      data: buffer,
      logicalName,
      filename,
      mimetype,
      source: role === "braindump" ? "inbound-braindump" : "inbound-dm",
      channel: chatId,
    });
    record = res.record;
    deduped = res.deduped;
  } catch (e) {
    d.log.error({ chatId, err: (e as Error).message }, "whatsapp: docstore add failed");
    reply(fill(d.texts.archiveFailed, { error: (e as Error).message }), "media-note", "archive-failed");
    return;
  }

  d.log.info(
    { chatId, role, id: record.id, name: record.logicalName, bytes: record.bytes, deduped },
    "whatsapp: inbound media archived",
  );

  // ADR-013: auto-enrich every NON-deduped archive (silent; keywords +
  // summary land in documents.db for find()). `priv:` opts out — those
  // bytes never enter any session.
  // A deduped record that was never enriched (a crash between the archive
  // commit and the enrich job) is enriched now; the job key makes a repeat
  // of this message a no-op.
  if (!decision.noEnrich && (!deduped || record.enrichStatus == null)) {
    d.fireEnrich(record, chatId, key("enrich"));
  }
  reply(
    fill(deduped ? d.texts.archivedDuplicate : d.texts.archived, {
      name: record.logicalName,
      id: record.id.slice(0, 8),
    }),
    "media-archived",
    "archived",
  );

  // Vault-import path (ADR-013, opt-in via vault:/import: caption, DM
  // only): same promotion wrapper as act — extracting a long PDF easily
  // outlives the quick window. runImport handles the identity-tag guard
  // internally (returns the refusal line as the reply).
  if (role === "dm" && decision.mode === "vault-import") {
    const jobKey = key("vault-import");
    const existing = jobKey ? d.jobStore.bySourceKey(jobKey) : null;
    if (existing) {
      replayImport(d, existing, record, reply);
      return;
    }
    await d.presence(chatId, "composing");
    const importPromise = d.runImport(record, chatId, jobKey);
    try {
      const raced = await d.quickWindow(importPromise);
      if (raced.settled) {
        reply(raced.value, "job-import", "import-result");
      } else {
        reply(d.texts.importStarted, "job-import", "import-started");
        importPromise.then(
          (resultLine) => reply(resultLine, "job-import", "import-result"),
          (e) =>
            reply(
              fill(d.texts.importFailed, { name: record.logicalName, error: (e as Error).message }),
              "job-import",
              "import-result",
            ),
        );
      }
    } catch (e) {
      reply(fill(d.texts.importFailedInWindow, { error: (e as Error).message }), "job-import", "import-result");
    } finally {
      await d.presence(chatId, "paused");
    }
    return;
  }

  // Act path: DM only, instruction-shaped caption. ADR-013: runs on a
  // ONE-SHOT JOB SESSION (own tmux session), so a 3-minute analysis can't
  // block the conversation. Timeout promotion: answer within the quick
  // window → single reply; else an acknowledgement now and the result when
  // the job finishes.
  if (role === "dm" && decision.mode === "act" && decision.instruction) {
    const jobKey = key("act");
    const existing = jobKey ? d.jobStore.bySourceKey(jobKey) : null;
    if (existing) {
      replayAct(d, existing, record, reply);
      return;
    }
    const docPath = d.docStore.pathFor(record);
    const framed = `[attached ${isImage ? "image" : "document"} "${record.logicalName}" archived at ${docPath} — use the Read tool on it]

${decision.instruction}`;
    await d.presence(chatId, "composing");
    const { jobId, promise } = d.startAct({
      record,
      chatId,
      instruction: decision.instruction.slice(0, 200),
      prompt: framed,
      sourceKey: jobKey,
    });
    try {
      const raced = await d.quickWindow(promise);
      if (raced.settled) {
        reply(raced.value.reply.trim() || d.texts.noResponse, "job-act", "act-result");
        d.log.info(
          { chatId, jobId, id: record.id, elapsedMs: raced.value.elapsedMs },
          "whatsapp: act-on-media replied in-window",
        );
      } else {
        d.jobStore.markPromoted(jobId);
        reply(d.texts.actStarted, "job-act", "act-started");
        d.log.info({ chatId, jobId, id: record.id }, "whatsapp: act-on-media promoted to deferred job");
        promise.then(
          (outcome) =>
            reply(
              fill(d.texts.actResult, { name: record.logicalName, reply: outcome.reply.trim() || d.texts.noResponse }),
              "job-act",
              "act-result",
            ),
          (e) => reply(fill(d.texts.actFailed, { name: record.logicalName, error: (e as Error).message }), "job-act", "act-result"),
        );
      }
    } catch (e) {
      // In-window failure (the job row is already marked failed by the
      // runner).
      const err = (e as Error).message;
      d.log.error({ chatId, jobId, err }, "whatsapp: act-on-media job failed in-window");
      reply(fill(d.texts.actFailedInWindow, { error: err }), "job-act", "act-result");
    } finally {
      await d.presence(chatId, "paused");
    }
  }
}

type Reply = (body: string, source: string, purpose: string) => void;

/** An act job this message already started: queue its result again (the
 *  queue drops it if it was queued). A running job's own handlers deliver;
 *  an orphaned one was announced at boot. */
function replayAct(d: MediaDeps, job: JobRow, record: DocRecord, reply: Reply): void {
  d.log.warn({ jobId: job.id, status: job.status }, "whatsapp: act job exists for this message — not started again");
  if (job.status === "done") {
    reply(
      fill(d.texts.actResult, { name: record.logicalName, reply: job.resultSummary?.trim() || d.texts.noResponse }),
      "job-act",
      "act-result",
    );
  } else if (job.status === "failed") {
    reply(fill(d.texts.actFailed, { name: record.logicalName, error: job.error ?? "" }), "job-act", "act-result");
  }
}

/** A vault-import job this message already started: queue its result again.
 *  The note is never written a second time. */
function replayImport(d: MediaDeps, job: JobRow, record: DocRecord, reply: Reply): void {
  d.log.warn({ jobId: job.id, status: job.status }, "whatsapp: import job exists for this message — not started again");
  const imported = d.docStore.get(record.id)?.importedPath ?? null;
  if (imported) {
    reply(importResultLine(record.logicalName, imported), "job-import", "import-result");
  } else if (job.status === "failed" || job.status === "done") {
    reply(
      fill(d.texts.importFailed, {
        name: record.logicalName,
        error: job.error ?? "the import stopped before the note was written; send the file again to retry",
      }),
      "job-import",
      "import-result",
    );
  }
}
