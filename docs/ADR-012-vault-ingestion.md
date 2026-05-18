# ADR-012 — Vault ingestion: PDFs/Word/HTML → markdown in the PARA tree

**Status:** Placeholder / deferred (2026-05-17)

This ADR is a stub. The design surfaced during ADR-008/009/010 discussions
but was not written up in full. The notes here capture the rough shape so a
future drafter doesn't start from zero.

## Problem

T3 (PARA-Obsidian) holds long-form knowledge as markdown. Anything else —
PDFs, Word docs, HTML clippings, Notion exports, email attachments — is
invisible to bots. The operator can't ask "what does the lease say about
pets?" because the file is a PDF the bot can't reach.

Khoj/Letta solve this with embeddings + semantic search. That path is
incompatible with the Claude Max billing principle (ADR-002, S4 deferred).

## Direction (subject to revision)

A new `vault-ingester` binary watches a drop directory, extracts text from
supported types via CPU-only tools, writes extracted markdown into the PARA
tree. Bots reach it through the existing `--add-dir` path. No vector store.

Rough sketch:

- **Drop dir:** `~/Nucleus-Inbox/` (env-configurable). Operator drags files in.
- **Trigger:** launchd `StartInterval=300`. Cheap polling.
- **Extractors:** `pdftotext` (poppler), `pandoc` (.docx, .html), `tesseract`
  (OCR fallback when pdftotext returns empty), passthrough for .txt/.md.
- **Output:** `3-Resources/imported/<date>-<slug>/<filename>.md`, frontmatter
  carries `original_filename`, `original_path`, `file_hash` (sha256, dedup
  key), `extraction_tool`, `ingested_at`.
- **Source disposition:** success → `~/Nucleus-Inbox/_processed/`; failure →
  `~/Nucleus-Inbox/_failed/` with sibling `.error.txt`.
- **Notification:** optional, configurable via `[ingest] notify_channels`
  in `nucleus.toml`. Defaults to silent.
- **No auto-classification** in MVP — everything lands in `3-Resources/
  imported/`. Operator moves to the right bucket manually, or invokes the
  brain-dump classification pipeline (ADR-005) post-ingest.

## Out of scope for this ADR

- Vector / embedding search (deferred per ADR-002)
- Auto-classification into PARA buckets
- Email-attachment / Drive-watch ingestion (extension of ADR-007)
- OCR-quality tuning beyond tesseract

## References

- ADR-002 — tiered memory, S4 deferral
- ADR-005 — PARA-Obsidian / brain-dump pipeline (downstream consumer of
  ingested files when classification is wanted)
- ADR-007 — Gmail/Calendar via MCP (potential future source of attachment
  ingestion)
