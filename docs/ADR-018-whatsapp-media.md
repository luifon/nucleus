# ADR-018 — WhatsApp media: inbound ingestion + outbound delivery

**Status:** Proposed
**Date:** 2026-06-01
**Related:** ADR-005b (WhatsApp DM mode), ADR-006 (reminders / `outbound_queue`),
ADR-008 (skills), ADR-016 (agent registry / run-log)

## Context

The WhatsApp surface is **text-only in both directions** today.

**Inbound** (`messaging/whatsapp/src/index.ts`):
- `extractText()` only reads `conversation`, `extendedTextMessage.text`, and the
  `caption` of `imageMessage` / `videoMessage`. Everything else returns `""` and
  is dropped at `if (!text.trim()) return;`.
- The one media type actually fetched is **audio** — `downloadMediaMessage` →
  `transcribe()` (Whisper) — but only because it collapses to text.
- Images and documents are silently discarded; their binaries are never
  downloaded. `Session.ask()` (`claude_session.ts`) takes a **string only** and
  pastes it into the tmux TUI, so there is no path to put a file in front of the
  model even if we had it.

**Outbound** (the `outbound_queue` table in `memory/whatsapp.db`, drained by
`startOutboundDrain` every 1s):
- Schema is text-body-only: `(id, target, body, source, enqueued_at, status,
  attempts, last_error, sent_at, msg_id)` — no media/mimetype/path column.
- The drain calls `sock.sendMessage(jid, { text: r.body })` and nothing else.
- Baileys **fully supports** media sends (`{ image }`, `{ document, mimetype,
  fileName }`, `{ video }`, `{ audio }`) — it's simply never been called.

The trigger for this ADR: a local image-generation model. The vision is that
Nucleus can generate an image (or assemble a document) and **deliver it to the
operator over WhatsApp** — and, symmetrically, that the operator can send an
image or PDF *to* Nucleus and have the model actually see it.

## Decision

Add media support as **three independent bricks**, so each can ship and be
tested on its own:

1. **Inbound media** — download image/document attachments, stage them on disk,
   and instruct the session to `Read` them (rather than pasting `@`-mentions).
2. **Outbound media** — extend `outbound_queue` with a `kind` + media columns;
   the drain branches to Baileys media shapes; a venue-correct enqueue CLI +
   skill lets a Claude session queue a file for delivery.
3. **Local image generation** — a separate model wrapped behind a CLI/skill that
   emits a file path; it feeds brick 2 and is otherwise decoupled.

The crux is brick 2's queue. Riding the **existing `outbound_queue`** (not a new
channel) is what lets slow generation never block a session: the session's last
act is to enqueue a row pointing at a file, and the 1s drain does the actual
send. The GPU and the WhatsApp socket are never coupled.

### Brick 1 — Inbound media

**New branch in `handleMessage()`**, alongside the existing `audioMessage` branch:

```
if (msg.message?.imageMessage || msg.message?.documentMessage) {
  // 1. downloadMediaMessage(msg, "buffer", …)  (same call audio already uses)
  // 2. write buffer to memory/whatsapp-media/inbound/<uuid>.<ext>  (TRANSIENT)
  //    ext derived from the message's mimetype
  // 3. caption (imageMessage.caption / documentMessage.caption) → the text body
  // 4. frame the message so the session is TOLD to Read the staged file
}
```

The `memory/whatsapp-media/inbound/` write is **transient ingest only** — the
place the session can `Read` the bytes from. The *durable* home is the Obsidian
vault, bound in via the brain-dump plan (see **Vault binding** below), exactly
the way text captures live in the vault, not in `memory/`.

**Decision: instruct-to-`Read`, not `@`-mention.** Claude Code's `@path` mention
is parsed by the TUI and is fragile through a bracketed `tmux paste-buffer` (the
same paste path that already needs `waitForInputSettled` to avoid eaten
Enters). Instead, `frame()` emits an explicit instruction and the session uses
its **Read tool**, which natively renders images and reads PDF pages:

```
[WhatsApp image — chat <id>]
The operator sent an image, staged at memory/whatsapp-media/inbound/<uuid>.png
Read that file to see it, then respond.
<caption, if any>
```

This is more robust than mention-parsing and uses a documented capability. The
staging dir lives under `memory/` (already inside the workspace and FDA-readable
by the bot — see the FDA-on-upgrade memory), so no `--add-dir` change is needed.

**Cleanup:** a sweeper deletes transient `inbound/` files older than 24h (the
session has read them, and any keeper has already been copied into the vault by
the plan). Mirror the brain-dump expiry sweep already in the codebase.

**Scope guard:** images (`image/*`) and documents (`application/pdf` and common
doc types) only. Video/sticker stay dropped — Claude can't usefully consume a
video, and stickers are noise. Log the dropped kind so it's visible.

### Brick 2 — Outbound media

**Schema migration (both writers).** The table is created identically in two
places — `messaging/whatsapp/src/db.ts` (TS, `node:sqlite`) and
`chores/reminders/src/store.rs` (Rust, `sqlx`). Both must gain the new columns,
and because `CREATE TABLE IF NOT EXISTS` won't alter an existing table, both must
run idempotent `ALTER TABLE … ADD COLUMN` migrations (guarded against the
"duplicate column" error so whichever process opens the DB first wins):

```sql
ALTER TABLE outbound_queue ADD COLUMN kind       TEXT NOT NULL DEFAULT 'text';
ALTER TABLE outbound_queue ADD COLUMN media_path TEXT;   -- abs path on disk
ALTER TABLE outbound_queue ADD COLUMN mimetype   TEXT;   -- document sends
ALTER TABLE outbound_queue ADD COLUMN filename   TEXT;   -- document display name
```

`kind ∈ {text, image, document}`. **`body` is reused as the caption** for media
rows (it's already `NOT NULL`; empty string = no caption) — no separate caption
column. Existing rows default to `kind='text'` and behave exactly as before.

**Drain branch** (`startOutboundDrain`, the per-row loop) replaces the single
`sendMessage(jid, { text })` with:

```
let content;
if (r.kind === "image")    content = { image: readFileSync(r.mediaPath), caption: r.body || undefined };
else if (r.kind === "document") content = { document: readFileSync(r.mediaPath), mimetype: r.mimetype, fileName: r.filename, caption: r.body || undefined };
else                       content = { text: r.body };
const sent = await sock.sendMessage(jid, content);
```

A missing/unreadable `media_path` → `markFailure` (do **not** bump the
connection-rot counter; that's reserved for `connection closed`). `OutboundRow`,
`OutboundQueueStore.enqueue()`, and `.pending()` gain the four new fields.

**Enqueue interface for the session.** Venue rule (Rule 7) puts this in
`messaging/whatsapp/`, mirroring `send.ts` / `ack.ts` which already use
`OutboundQueueStore`. Two layers:

- A CLI — `messaging/whatsapp/src/enqueue-media.ts` (run as `node` against the
  built JS) — args `--kind image|document --path <abs> [--caption …]
  [--mimetype …] [--filename …] [--target whatsapp-dm]`. It inserts one media
  row. `target` defaults to the operator DM (first `WHATSAPP_ALLOWED_DM_JIDS`).
- A thin **skill** (`~/.claude/skills/whatsapp-send-media/`, ADR-008) wrapping
  the CLI, so a session can invoke it ergonomically by name and it carries a
  `# Failure modes` section. Personal tree, not committed (Rule 11).

**Authorization is enforced at the drain, not the enqueue** (defense in depth):
`resolveOutboundTarget()` already refuses any JID not on the allowlist, so even a
mis-targeted enqueue can't escape to an arbitrary chat. Delivering a generated
image to the operator's own DM is pre-authorized under Rule 6 / ADR-005b; any
*other* destination still requires explicit user authorization and the skill
must not default elsewhere.

**Cleanup:** the file referenced by a media row is a *send* artifact, not the
durable copy. If it's a keeper it was already bound into the vault (see **Vault
binding**); the queue only needs it until `markSent`, after which a sweep of the
outbound staging dir (older than 24h) reclaims it.

### Brick 3 — Local image generation

Out of scope for the heavy lift here; it only has to satisfy a contract: a CLI
`gen-image --prompt "…" --out <abs.png>` that **blocks until the file exists**.
Nucleus wraps it in a skill that (1) runs the model, (2) calls the brick-2
enqueue CLI with the resulting path. The model choice (the local-model news item
that prompted this) is an implementation detail behind that CLI.

### Vault binding — media lives where every other capture lives

Media is **not** a special citizen with its own storage scheme. A binary worth
keeping binds into the Obsidian vault through the **same brain-dump multi-op plan
(Rule 9)** that files text captures — same PARA bucket routing, same
append-over-create, same sibling-linking. `memory/whatsapp-media/` is only
transient ingest/send scratch; the vault is the home.

The brain-dump `CaptureOp` union (`messaging/whatsapp/src/braindump.ts:43`) is
**text-only today** — `create`/`append` carry a markdown `body`, `move` renames.
Binding media therefore requires a **new op kind**:

```
| {
    op: "attach";
    sourcePath: string;        // transient path the binary currently sits at
    bucket: string;            // SAME bucket vocabulary as `create`
    filename: string;          // leaf name inside the bucket, e.g. "screenshot.png"
    createsSubfolder: boolean; // gated identically to `create`/`move`
    embedInPath?: string;      // a note (create/append target) to embed ![[filename]] into
    reason: string;
  }
```

`attach` copies the binary into the destination **bucket** (co-located with the
notes there — "the same places other stuff goes"), and, when `embedInPath` is
set, inserts an Obsidian embed (`![[filename]]`) into that note so the image/PDF
renders inline. It obeys every Rule-9 guard the text ops do: bucket validity,
the `createsSubfolder` gate, append-over-create, can't-classify → `0-Inbox/`.

**Persistence default: explicit "save this."** A binary the operator sends over
is **not** filed by default — the session Reads it for context and the transient
copy is swept. The `attach` op only fires on an explicit save intent ("save
this", "keep this", "file that screenshot"). This holds across roles, including
brain-dump: a captured *thought* still files as text, but its attached image is
persisted only when asked. Rationale: most images sent over are
ask-about-this-screenshot, not keepers; auto-filing every one clutters the vault.

Routing, then:

- **Inbound media, any role** → Read for context, transient by default. On an
  explicit save intent, emit an `attach` op (for a brain-dump capture, paired
  with the `create`/`append` that holds the surrounding text + the `![[…]]`
  embed) so the binary lands in the chosen bucket.
- **Kept outbound artifacts** (a generated image the operator says "save this")
  → the same `attach` op binds it into the vault. Delivery (the queue) and
  archival (the vault) stay separate concerns; an image can be sent without being
  saved, and saved without being re-sent.

**Where the binary lands.** The vault currently sets no `attachmentFolderPath`
and has no embedded media yet (`.obsidian/app.json` only carries the `_`-prefix
ignore filter), so this ADR *establishes* the convention:

- **Default: co-located in the destination bucket**, embedded by wiki-embed
  (`![[filename]]`).
- **Flat, high-churn buckets** (`2-Daily-Notes`, `6-Slipbox`): the binary goes in
  a `_attachments/` sibling instead of inline, so it doesn't clutter the graph.
  The `_` prefix is already graph/search-ignored per `app.json`, so the embed
  still renders in the note while the file stays out of the graph. The `attach`
  applier picks co-located vs `_attachments/` from the destination bucket.

### The timeout question — decouple, don't inflate

The operator's instinct was "raise the session timeout." The send itself is
fast; the slow part is generation. Two patterns, by who's waiting:

- **Nobody waiting** (reminder-fired / proactive "make and send me X"): let the
  fire session run generation synchronously with a generous `maxWaitMs`, then
  enqueue. Simple, fine — no user is blocked.
- **Operator waiting in the DM** ("make me an image now"): the session replies
  immediately with text ("on it — sending shortly"), and generation runs
  **detached**; its final step enqueues the media row. The 1s drain delivers the
  image whenever it's ready. The conversational `ask()` never holds for minutes.

Either way `maxWaitMs` on the *conversational* path stays at today's 180s. Only
the detached/fire generation step gets a long budget, and it isn't on the
critical path of a reply. This is the whole reason brick 2 rides the queue.

## Rollout phases

1. **Inbound Read (brick 1a).** Add the image/document branch + transient staging
   + sweeper + `frame()` instruction. Lowest risk, no schema change, immediately
   useful (send Nucleus a screenshot/PDF and ask about it). Validate the
   `Read`-tool path end-to-end from a live DM.
2. **Vault binding (`attach` op).** Extend the `CaptureOp` union + the plan
   validator + applier with the `attach` opcode (copy-into-bucket + optional
   `![[…]]` embed), obeying the existing Rule-9 gates. This is the shared
   primitive both inbound keepers and outbound keepers route through. Validate by
   capturing a braindump image and confirming it lands embedded in the right
   bucket.
3. **Outbound queue (brick 2).** Schema migration on both writers; drain branch;
   `OutboundRow`/store changes; enqueue CLI; skill wrapper. Validate by enqueuing
   a static test PNG and confirming delivery to the operator DM. Verify legacy
   `kind='text'` rows (reminders) still send unchanged.
4. **Generation (brick 3).** Stand up the local model behind `gen-image`; wire
   the generate→enqueue skill; choose synchronous-vs-detached per the timeout
   section. Validate the detached path doesn't stall the conversational reply.

Phase 1a needs nothing. Phase 2 (the `attach` op) is what makes media durable and
gates filing any keeper, inbound or outbound. Phases 1a, 2, and 3 are otherwise
independent; phase 4 (generation) depends on phase 3 (the queue).

## Consequences

- `outbound_queue` becomes the single typed delivery primitive for *all* WhatsApp
  output (text, image, document) — reminders, brain-dump acks, and generated
  media all ride one drain with one auth gate.
- Media has no parallel storage scheme: durable copies live in the vault via the
  `attach` op, alongside every text capture. The cost is extending the brain-dump
  `CaptureOp` contract (type + validator + applier), and that union is now the
  one place media filing converges — inbound keepers and outbound keepers share it.
- Two DB writers must keep the migration in lockstep; document the column set in
  both files so they don't drift (they already note the cross-process contract).
- Inbound media adds disk churn under `memory/whatsapp-media/`; bounded by the
  24h sweeper. Large PDFs/images cost session context when `Read`.
- Baileys media uploads can be slower/flakier than text; the existing
  per-row retry (`OUTBOUND_MAX_ATTEMPTS`) covers it, but a stuck large upload
  could slow a drain tick — the 20-row batch cap already bounds that.

## Open questions

- **Document inbound breadth:** PDF is clearly worth it (Read supports pages).
  Office formats (`.docx`, `.xlsx`) aren't natively rendered — convert, or drop
  with a note? Lean drop-with-note initially.
- **Save-intent detection:** the explicit-"save this" default needs the planning
  session to reliably recognize save intent ("save this", "keep that", "file it")
  vs an ask-about-this image. Worth a few phrasings in the brain-dump persona /
  `attach`-op prompt so it doesn't under- or over-trigger.
- **Voice-note *output*** (`{ audio, ptt: true }`): natural future extension
  (TTS replies) once brick 2's `kind` enum exists — not in this ADR.
