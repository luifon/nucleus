# ADR-018 — WhatsApp media + personal document library (local library)

**Status:** Accepted (shipped 2026-06-12) — third revision; supersedes the
encrypted-Drive draft (2026-06-01), which superseded the vault-binding draft.
**Date:** 2026-06-12
**Related:** ADR-005b (WhatsApp DM mode), ADR-006 (reminders / `outbound_queue`),
ADR-011 (tailnet perimeter — the dashboard surface's access gate),
ADR-015 (dashboard), ADR-019 (image-generation gallery — a producer for the
outbound media path), ADR-020 (DB ownership rule, typegen, drain hang
protection — all load-bearing here).

## Context

WhatsApp was **text-only in both directions**:

- **Inbound**: `extractText()` read only `conversation` / `extendedTextMessage`
  / image|video `caption`; everything else dropped. Audio was the lone media
  type fetched (`downloadMediaMessage` → Whisper) because it collapses to text.
- **Outbound** (`outbound_queue` in `memory/whatsapp.db`, drained 1s): text-only
  schema; the drain called `sock.sendMessage(jid, { text })`.

The goal: **WhatsApp as a working surface for documents**, headlined by
on-demand retrieval — "send me my RG / CPF / passport" from the DM, file
delivered. Generated images (ADR-019) become one producer feeding the same
outbound path.

**Revision history on the storage model** (the part that changed twice):

1. *Vault-binding draft*: binaries inside the Obsidian vault → dropped
   (binary churn, graph clutter, and the vault is `--add-dir`'d into LLM
   sessions).
2. *Encrypted-Drive draft*: client-side-encrypted binaries on the trash
   account's Drive → dropped (this revision). Two reasons: implementation
   reality and a better trade. Service accounts **cannot access a personal
   Gmail's My Drive** (Workspace-only delegation), so the headless path
   degraded to rclone+OAuth — and by the draft's own admission the Drive
   plumbing was "where most of the build effort concentrates". All of it
   bought a remote copy on a throwaway account that then *required*
   client-side encryption (key management, Keychain, lost-key footgun) to
   be tolerable.
3. **This revision: local library.** Operator decision 2026-06-12. The
   from-anywhere use case never needed Drive — WhatsApp delivery IS the
   from-anywhere mechanism, and the bot runs on this Mac: if the Mac is
   off, all of Nucleus is off, Drive or not.

## Decision — three planes + two cross-cutting rules

- **Storage = local library.** Bytes at `memory/documents/<id>.<ext>`,
  metadata in `memory/documents.db` (`documents` + `doc_audit` tables).
  The TS docstore (`messaging/whatsapp/src/docstore.ts`) owns ALL writes
  (ADR-020 ownership — the bot process + its CLIs are one family; WAL +
  busy_timeout absorb same-family concurrency). Everything else reads:
  the dashboard opens read-only via `core::db::open_read_only()` (never
  `db::open` — `create_if_missing` would conjure an empty foreign DB).
- **Index/audit = Obsidian text layer** (`4-Areas/Documents/`) — never the
  bytes. See below.
- **Transport = WhatsApp** — inbound via `downloadMediaMessage`; outbound
  via the media-extended `outbound_queue`.

Cross-cutting, carried from the prior drafts and still load-bearing:

- **By-reference orchestration** (privacy-critical). The model resolves
  *which* file (name/tags → id via the docstore); the plumbing moves the
  bytes disk → staging → Baileys. **Document bytes never enter model
  context or session transcripts.** Inbound is symmetric: the model only
  Reads an inbound file when the operator explicitly asks it to act on one.
- **DM-lock** (misdelivery is the catastrophic failure mode). Three layers
  make a wrong destination *inexpressible*: the retrieval skill forbids
  naming a target → `enqueue-media.ts --doc` **parses no target flag at
  all** (operator DM derived inside the CLI from `WHATSAPP_ALLOWED_DM_JIDS`)
  → the drain's `resolveOutboundTarget` allowlist re-validates.

### Why not the vault (unchanged)

Beyond churn/clutter: the vault is mounted into LLM sessions constantly
(braindump, chat, distiller all get `--add-dir <vault>`). Identity documents
inside it would be one `Read` away from any session — the by-reference rule
dies. `memory/documents/` is deliberately NOT handed to sessions wholesale.

### Why no encryption (new)

The encryption defended against compromise of a cloud account that no longer
holds the documents. Locally, FileVault covers at-rest; a fully-compromised
Mac was already game-over in the prior draft's own threat model (it holds
`.env`, every token, the WhatsApp session). App-layer encryption here would
add a key-management surface and a lost-key footgun for zero marginal
protection.

### Durability — consciously deferred

Single-disk: Mac loss/disk death loses the library until the future mirror
exists. Owned in writing, accepted by the operator (originals exist
physically). Cheap interim mitigations: keep `memory/documents*` inside the
Time Machine scope; the vault `audit.md` (append-only) survives a
`documents.db` loss as a human-readable inventory trail.

**Future mirror (replaces the Drive section):** `WHATSAPP_DOCUMENTS_DIR` is
the seam — point it at a mounted external volume (or rsync nightly to a
self-hosted box) and the library moves; the DB stays at
`memory/documents.db`. If the mirror target ever leaves the machine,
encrypt at mirror time (age). A restore drill is required before any mirror
counts as durability. Out of S18 scope.

## The pieces (as shipped)

### Outbound queue — media extension

`outbound_queue` gains `kind ∈ {text,image,document}` (body doubles as
caption), `media_path`, `mimetype`, `filename`. TS heals existing DBs via
`PRAGMA table_info` detection; the Rust co-creator
(`chores/reminders/src/store.rs::open_whatsapp_db`) carries the full CREATE
for fresh-install parity only — no Rust migrations on whatsapp.db (ADR-020).

**Lifecycle contract:** `media_path` is a **drain-owned staged file** under
`memory/outbound-staging/` — `enqueue-media` always COPIES into staging
(pointing at a library original would let the drain delete your passport).
The drain unlinks ONLY at terminal state: `markSent`, or a `markFailure`
that *returns* `failed` (the return type exists for exactly this), or
`markFailedTerminal` (missing/oversized file — retrying can never succeed).
A retried row's file survives. Boot sweep collects staging orphans.

Send budget: text 20s / media 90s timeouts, max 3 media sends per tick,
`DRAIN_WATCHDOG_MS` derived from those constants (ADR-020's watchdog,
now drift-proof). Size cap `WHATSAPP_MEDIA_MAX_BYTES` (default 64MB).
Baileys `{url}` stream form — no multi-MB buffers in heap.

### Document store

`documents(id, logical_name, tags JSON, filename, ext, mimetype, bytes,
sha256, source, added_at, last_retrieved_at, retrieve_count, status)` +
`doc_audit(doc_id, action, channel, detail, at)`. Atomic add
(tmp→fsync→rename→tx): no row ever points at a missing file. sha256 dedup
(re-sending your passport doesn't double-store). Soft delete. Exact-first
`find()` tiers: exact name → exact tag → substring → token fuzzy.

**Stored-name contract (normative):** the on-disk file is
`${id}.${lower(ext(filename))}` (mimetype-map fallback, then `bin`).
`docstore.ts::storedName()` is the one implementation; the dashboard
composes from the API's `ext` field. Don't fork the logic.

### Vault text layer (`4-Areas/Documents/`)

- `Documents-overview.md` — hub, written once if missing (operator-curated
  afterwards).
- `manifest.md` — a **regenerated view** of documents.db (full rewrite on
  every mutation incl. retrievals; tmp+rename atomic; do-not-edit callout;
  dataview inline fields). Regeneration makes rename/retag corruption
  structurally impossible and the file self-healing.
- `audit.md` — **append-only** (monthly headings). Survives DB loss.

Written by direct deterministic fs writes (diary.ts precedent) — NOT the
braindump review pipeline: fixed paths, DB-derived content, nothing for a
model to decide. Don't "fix" this into the op pipeline. (Rule 9: the
sub-folder creation is operator-directed via this ADR.)

### Inbound (operator → Nucleus)

Every inbound image/document **archives to the library** (dedup absorbs
re-sends), in DM and braindump roles; ack carries the stored name + id.
`documentWithCaptionMessage` is normalized (captioned docs silently drop
otherwise). Caption heuristics (`caption.ts`): `name:`/`nome:` → archive
with that name; `act:`/`faz:`/`!` → force act; ≤5 words with no sentence
punctuation → it's a label; anything else **archives AND acts** (a false
act wastes one turn; a false archive-only forces a re-ask that still
works). The act path hands the session the **docstore path** to Read — no
staging copy; the archived file is already local and under the workspace.
Braindump role is capture-only (archive + ack, never a session ask).

### Outbound retrieval — the headline

DM: "send me my RG" → the `send-document` skill (repo-committed,
`.claude/skills/send-document/`) → `docs.ts find` → 0/1/many handling (one
disambiguation question max; never guess between identity documents) →
`enqueue-media.ts --doc <id>` → drain delivers to the DM. Audited at every
hop (retrieve_count + doc_audit + vault audit.md). The DM session pool
pre-approves the two CLIs (`PoolConfig.allowedTools`) and carries a
code-owned capability blurb on top of the operator-owned persona.

### Dashboard surface (`/documents`)

Read-only viewer (library grid + audit trail) over the TS-owned DB.
File bytes served at `/documents/files/` with **`Cache-Control: no-store`**
(identity bytes never persist in a browser cache) + `nosniff`, behind the
ADR-011 tailnet perimeter. Dashboard views deliberately do NOT bump
retrieve_count — "retrieved" means "delivered to the operator", not
"looked at". Per-request auth was considered and rejected: at the tailnet
trust level it's theater (an attacker who can reach the dashboard can read
`memory/` through the chat surface anyway).

## Consequences

- `outbound_queue` is the single media-delivery primitive (reminders,
  gallery, doc retrieval) with one auth gate at the drain.
- The library is one `cp -r` to back up; zero key management.
- Durability risk is single-disk and owned in writing (mitigations above);
  the future mirror has a named seam (`WHATSAPP_DOCUMENTS_DIR`).
- The dashboard now serves identity files inside the tailnet perimeter
  (no-store; env-gating the file route is a one-line follow-up if the
  posture ever feels wrong).
- Sensitive-document delivery remains a high-consequence path: the
  three-layer DM-lock, by-reference handling, and the dual audit trail
  are load-bearing, not nice-to-have.

## Open questions

- Office formats inbound (`.docx`/`.xlsx`): Read renders neither — store-only
  for now; convert-on-act is a follow-up if it ever matters.
- Mirror target (external volume vs self-hosted) — decided when the
  durability deferral is revisited.
