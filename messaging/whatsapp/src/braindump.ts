// Brain-dump multi-op pipeline.
//
// Each capture goes through Claude (with the vault as --add-dir). Claude
// returns a list of OPERATIONS to apply to the vault — create new files,
// append fragments to existing files, move/rename existing files. The TS
// code validates each op (path-escape, vault containment, sub-folder
// creation gating) and applies them in order.
//
// Why multi-op: a 6-minute audio about a work contract isn't ONE thing —
// it's contract terms + team info + funnel notes + tooling matrix. Filing
// it as a single 4KB markdown is a worse outcome than splitting into
// themed siblings under a project folder. The bot's job is to do that
// split, not punt to the user.
//
// CLAUDE.md Rule 9 governs:
//  - Sub-folder creation under 1-Projects/2-Areas/3-Resources requires
//    `createsSubfolder: true` AND must be justified by an explicit
//    directive in the capture itself ("create a folder for X", "put this
//    in Projects/Y").
//  - Never invent folder names. Speculative creation is forbidden;
//    the safe path is 0-Inbox.

import fs from "node:fs";
import path from "node:path";
import { Session, type SpawnOptions } from "./claude_session.js";
import type { Config } from "./config.js";

// ==================== TYPES ====================

export type CaptureOp =
  | {
      op: "create";
      bucket: string;             // e.g. "1-Projects/Example-Project"
      filename: string;           // leaf name
      body: string;               // markdown w/ frontmatter
      createsSubfolder: boolean;  // true if bucket sub-folder doesn't exist yet
      reason: string;
    }
  | {
      op: "append";
      targetPath: string;         // relative to vault, MUST exist
      body: string;               // appended with dated separator
      reason: string;
    }
  | {
      op: "move";
      fromPath: string;           // relative to vault, MUST exist
      toBucket: string;           // destination bucket
      toFilename: string;         // empty = keep original filename
      createsSubfolder: boolean;
      reason: string;
    };

export interface AppliedOp {
  op: "create" | "append" | "move";
  status: "ok" | "rejected";
  /** Path relative to vault on success. For `move`, the destination. */
  resultPath?: string;
  /** Source path for `move` ops on success. */
  fromPath?: string;
  /** Why we rejected, if status === "rejected". */
  rejection?: string;
  /** Claude's reason for the op (always present). */
  reason: string;
}

export interface CaptureOutcome {
  ops: AppliedOp[];
  summary: string;
  confidence: number;
  elapsedMs: number;
}

interface ClaudePlan {
  ops: CaptureOp[];
  summary: string;
  confidence: number;
}

// ==================== CONSTANTS ====================

const FALLBACK_BUCKET = "0-Inbox";
const ALLOWED_TOPS = [
  "0-Inbox",
  "1-Projects",
  "2-Areas",
  "3-Resources",
  "4-Archives",
];
/** Top-level dirs whose sub-folders are durable user commitments — bots
 *  must NOT auto-create them unless the capture explicitly directed it
 *  (signalled by createsSubfolder: true). */
const NEEDS_DIRECTIVE_FOR_SUBFOLDER = new Set([
  "1-Projects",
  "2-Areas",
  "3-Resources",
]);

// ==================== ENTRY ====================

export async function captureToPara(
  text: string,
  inputKind: "text" | "voice",
  config: Config,
): Promise<CaptureOutcome> {
  const t0 = Date.now();
  const vaultSummary = summarizeVault(config.vaultPath);
  const today = new Date().toISOString().slice(0, 10);

  const prompt = buildPrompt(text, inputKind, config.vaultPath, vaultSummary, today);

  const spawnOpts: SpawnOptions = {
    workspaceRoot: config.workspaceRoot,
    appendSystemPrompt: config.appendSystemPrompt,
    permissionMode: config.permissionMode,
    disallowedTools: config.disallowedTools,
    addDirs: [config.vaultPath],
    tmuxSession: "nucleus-whatsapp-braindump",
    windowName: `cap-${today}`,
  };

  const session = await Session.spawn(spawnOpts);
  let raw: string;
  try {
    raw = await session.ask(prompt);
  } finally {
    await session.close().catch(() => {});
  }

  const plan = parsePlan(raw);

  // Apply each op, collecting results. Continue past rejections so a single
  // bad op doesn't void the whole plan.
  const applied: AppliedOp[] = [];
  for (const op of plan.ops) {
    applied.push(applyOp(config.vaultPath, op));
  }

  // Safety net: if Claude returned no ops OR every op was rejected, drop
  // the raw capture into 0-Inbox so we never silently lose the user's data.
  const anySuccess = applied.some((a) => a.status === "ok");
  if (!anySuccess) {
    const fallbackOp: CaptureOp = {
      op: "create",
      bucket: FALLBACK_BUCKET,
      filename: `${today}-fallback-${Date.now().toString(36)}.md`,
      body: synthesizeFallbackBody(today, text, plan, applied),
      createsSubfolder: false,
      reason: "all proposed ops rejected; preserving capture for manual sort",
    };
    applied.push(applyOp(config.vaultPath, fallbackOp));
  }

  return {
    ops: applied,
    summary: plan.summary,
    confidence: plan.confidence,
    elapsedMs: Date.now() - t0,
  };
}

// ==================== APPLY ====================

function applyOp(vaultPath: string, op: CaptureOp): AppliedOp {
  switch (op.op) {
    case "create":
      return applyCreate(vaultPath, op);
    case "append":
      return applyAppend(vaultPath, op);
    case "move":
      return applyMove(vaultPath, op);
  }
}

function applyCreate(
  vaultPath: string,
  op: Extract<CaptureOp, { op: "create" }>,
): AppliedOp {
  const resolved = resolveBucket(vaultPath, op.bucket, {
    allowSubfolderCreate: op.createsSubfolder,
  });
  if (!resolved.ok) {
    return { op: "create", status: "rejected", reason: op.reason, rejection: resolved.reason };
  }
  fs.mkdirSync(resolved.path, { recursive: true });
  const leaf = sanitizeFilename(op.filename) || `capture.md`;
  const target = path.join(resolved.path, leaf);
  if (fs.existsSync(target)) {
    appendWithSeparator(target, op.body);
  } else {
    fs.writeFileSync(target, op.body.trim() + "\n");
  }
  return {
    op: "create",
    status: "ok",
    resultPath: path.relative(vaultPath, target),
    reason: op.reason,
  };
}

function applyAppend(
  vaultPath: string,
  op: Extract<CaptureOp, { op: "append" }>,
): AppliedOp {
  const valid = validateExistingFile(vaultPath, op.targetPath);
  if (!valid.ok) {
    return { op: "append", status: "rejected", reason: op.reason, rejection: valid.reason };
  }
  appendWithSeparator(valid.absPath, op.body);
  return {
    op: "append",
    status: "ok",
    resultPath: path.relative(vaultPath, valid.absPath),
    reason: op.reason,
  };
}

function applyMove(
  vaultPath: string,
  op: Extract<CaptureOp, { op: "move" }>,
): AppliedOp {
  const fromValid = validateExistingFile(vaultPath, op.fromPath);
  if (!fromValid.ok) {
    return { op: "move", status: "rejected", reason: op.reason, rejection: `from: ${fromValid.reason}` };
  }
  const dest = resolveBucket(vaultPath, op.toBucket, {
    allowSubfolderCreate: op.createsSubfolder,
  });
  if (!dest.ok) {
    return { op: "move", status: "rejected", reason: op.reason, rejection: `to-bucket: ${dest.reason}` };
  }
  fs.mkdirSync(dest.path, { recursive: true });
  const leaf = sanitizeFilename(op.toFilename) || path.basename(fromValid.absPath);
  const destPath = path.join(dest.path, leaf);
  if (destPath === fromValid.absPath) {
    return { op: "move", status: "rejected", reason: op.reason, rejection: "no-op move (same path)" };
  }
  if (fs.existsSync(destPath)) {
    return {
      op: "move",
      status: "rejected",
      reason: op.reason,
      rejection: `destination already exists: ${path.relative(vaultPath, destPath)}`,
    };
  }
  fs.renameSync(fromValid.absPath, destPath);
  return {
    op: "move",
    status: "ok",
    fromPath: path.relative(vaultPath, fromValid.absPath),
    resultPath: path.relative(vaultPath, destPath),
    reason: op.reason,
  };
}

// ==================== HELPERS ====================

function appendWithSeparator(absPath: string, fragment: string): void {
  const today = new Date().toISOString().slice(0, 10);
  const existing = fs.readFileSync(absPath, "utf-8");
  const trimmed = fragment.trim();
  // HTML comment marker — invisible when rendered, searchable in raw.
  fs.writeFileSync(
    absPath,
    `${existing.trimEnd()}\n\n<!-- appended ${today} via alfred-braindump -->\n\n${trimmed}\n`,
  );
}

interface PathOk {
  ok: true;
  absPath: string;
}
interface PathReject {
  ok: false;
  reason: string;
}

/** Validate that `relPath` points to an existing file inside the vault. */
function validateExistingFile(vaultPath: string, relPath: string): PathOk | PathReject {
  const cleaned = (relPath ?? "").trim().replace(/^\/+|\/+$/g, "");
  if (!cleaned) return { ok: false, reason: "empty path" };
  if (cleaned.includes("..") || cleaned.startsWith("/")) {
    return { ok: false, reason: `path-escape attempt: ${cleaned}` };
  }
  const top = cleaned.split("/")[0];
  if (!ALLOWED_TOPS.includes(top)) {
    return { ok: false, reason: `path top is not a vault bucket: ${top}` };
  }
  const abs = path.join(vaultPath, cleaned);
  if (!fs.existsSync(abs)) {
    return { ok: false, reason: `file does not exist: ${cleaned}` };
  }
  if (!fs.statSync(abs).isFile()) {
    return { ok: false, reason: `not a file: ${cleaned}` };
  }
  return { ok: true, absPath: abs };
}

interface BucketOk {
  ok: true;
  path: string;
}
interface BucketReject {
  ok: false;
  reason: string;
}

/** Resolve a bucket string to an absolute vault path. Sub-folders under
 *  Projects/Areas/Resources can only be created when `allowSubfolderCreate`
 *  is true (which Claude sets via `createsSubfolder` on the op, justified
 *  by an explicit directive in the capture). */
function resolveBucket(
  vaultPath: string,
  bucket: string,
  opts: { allowSubfolderCreate: boolean },
): BucketOk | BucketReject {
  const raw = (bucket ?? "").trim().replace(/^\/+|\/+$/g, "");
  if (!raw) return { ok: false, reason: "empty bucket" };
  if (raw.includes("..") || raw.startsWith("/")) {
    return { ok: false, reason: `path-escape attempt: ${raw}` };
  }
  const top = raw.split("/")[0];
  if (!ALLOWED_TOPS.includes(top)) {
    return { ok: false, reason: `unknown top-level bucket: ${top}` };
  }
  const target = path.join(vaultPath, raw);
  const isSubfolder = raw.includes("/");
  const needsDirective = NEEDS_DIRECTIVE_FOR_SUBFOLDER.has(top) && isSubfolder;
  if (needsDirective && !fs.existsSync(target) && !opts.allowSubfolderCreate) {
    return {
      ok: false,
      reason: `sub-folder ${raw} doesn't exist and createsSubfolder=false (won't auto-create)`,
    };
  }
  return { ok: true, path: target };
}

function sanitizeFilename(name: string): string {
  return (name ?? "").replace(/[/\\\0]/g, "").trim();
}

function synthesizeFallbackBody(
  today: string,
  capture: string,
  plan: ClaudePlan,
  attempted: AppliedOp[],
): string {
  const failures = attempted
    .filter((a) => a.status === "rejected")
    .map((a) => `- **${a.op}**: ${a.rejection ?? "no reason"} (claude said: ${a.reason})`)
    .join("\n");
  return `---
created: ${today}
source: alfred-braindump
tags: [fallback, needs-manual-sort]
---

# Fallback capture (${today})

The brain-dump pipeline tried to file this as a multi-op plan but no op
was applied successfully. The raw capture is preserved below for manual
sorting.

## Plan summary
${plan.summary || "(no summary)"}

## Why every op failed
${failures || "(no ops attempted)"}

## Original capture

${capture}
`;
}

// ==================== PARSE ====================

function parsePlan(raw: string): ClaudePlan {
  const cleaned = raw
    .trim()
    .replace(/^```json\s*/i, "")
    .replace(/^```\s*/, "")
    .replace(/\s*```$/, "")
    .trim();
  let obj: any;
  try {
    obj = JSON.parse(cleaned);
  } catch (e) {
    throw new Error(
      `braindump: claude output was not valid JSON: ${(e as Error).message}\n--- raw ---\n${raw}`,
    );
  }
  if (!Array.isArray(obj.ops)) {
    throw new Error(
      `braindump: missing ops[] in claude output: ${cleaned.slice(0, 200)}`,
    );
  }

  const ops: CaptureOp[] = [];
  for (const rawOp of obj.ops) {
    if (!rawOp || typeof rawOp.op !== "string") continue;
    const reason = typeof rawOp.reason === "string" ? rawOp.reason : "(no reason)";

    if (rawOp.op === "create") {
      if (
        typeof rawOp.bucket !== "string" ||
        typeof rawOp.filename !== "string" ||
        typeof rawOp.body !== "string"
      ) continue;
      ops.push({
        op: "create",
        bucket: rawOp.bucket,
        filename: rawOp.filename,
        body: rawOp.body,
        createsSubfolder: !!rawOp.createsSubfolder,
        reason,
      });
    } else if (rawOp.op === "append") {
      if (typeof rawOp.targetPath !== "string" || typeof rawOp.body !== "string") continue;
      ops.push({
        op: "append",
        targetPath: rawOp.targetPath,
        body: rawOp.body,
        reason,
      });
    } else if (rawOp.op === "move") {
      if (typeof rawOp.fromPath !== "string" || typeof rawOp.toBucket !== "string") continue;
      ops.push({
        op: "move",
        fromPath: rawOp.fromPath,
        toBucket: rawOp.toBucket,
        toFilename: typeof rawOp.toFilename === "string" ? rawOp.toFilename : "",
        createsSubfolder: !!rawOp.createsSubfolder,
        reason,
      });
    }
  }

  return {
    ops,
    summary: typeof obj.summary === "string" ? obj.summary : `${ops.length} ops`,
    confidence: typeof obj.confidence === "number" ? obj.confidence : 0.5,
  };
}

// ==================== PROMPT ====================

function buildPrompt(
  text: string,
  inputKind: "text" | "voice",
  vaultPath: string,
  vaultSummary: string,
  today: string,
): string {
  return `You are filing a brain-dump capture into the user's PARA-organized
Obsidian vault at ${vaultPath}. The vault is mounted via --add-dir, so
read freely: README.md in each bucket explains what belongs there;
existing notes show what's already covered.

Capture (from ${inputKind}, ${today}):
---
${text}
---

You will output a SINGLE JSON object describing OPERATIONS on the vault
— not just a single file. The capture may decompose into multiple files
across multiple folders, may append to existing notes, and may include
moves of prior misfiled notes. See OPS below.

OUTPUT SHAPE:

{
  "ops": [
    {
      "op": "create",
      "bucket": "1-Projects/Example-Project",
      "filename": "${today}-contract.md",
      "body": "<full markdown body INCLUDING YAML frontmatter>",
      "createsSubfolder": false,
      "reason": "<one sentence>"
    },
    {
      "op": "append",
      "targetPath": "1-Projects/Example-Project/contract.md",
      "body": "<fragment to append; bot adds a dated separator>",
      "reason": "<one sentence>"
    },
    {
      "op": "move",
      "fromPath": "0-Inbox/some-file.md",
      "toBucket": "1-Projects/Example-Project",
      "toFilename": "",
      "createsSubfolder": false,
      "reason": "<one sentence>"
    }
  ],
  "summary": "1-line user-facing summary like 'created 3 docs in Projects/Example-Project, moved 1'",
  "confidence": 0.0..1.0
}

DECOMPOSITION (the most important rule):

1. Identify the major themes in the capture. ONE FILE PER MAJOR THEME.
   For a long capture (~5+ minutes of audio, ~3000+ chars), this is
   typically 1-3 files. For a short capture, just 1. Use sub-headings
   inside each file to separate sub-themes — don't atomize into
   micro-notes (Zettelkasten-style is NOT what we want).

   Example: a 6-minute audio about a work contract decomposes into
   maybe (contract terms / company background / your role / tooling) —
   3-4 files, not 1, not 15.

2. Strongly prefer APPEND over CREATE when an existing file already
   covers a theme. Look at existing notes' titles + frontmatter; if a
   captured fragment overlaps, append instead of duplicating.

3. The capture may be a META-CORRECTION ("that thing earlier should be
   in Projects/X, not Inbox", "rename that file", "decompose what I
   sent before"). Detect this and use \`move\` ops to actually relocate
   the prior file. Look at recent files in 0-Inbox and the buckets to
   identify what's being corrected. The correction does the work — do
   NOT file a new note describing what should happen.

PLACEMENT (CLAUDE.md Rule 9):

4. Use existing folders when one fits. If nothing fits AND the capture
   EXPLICITLY directs creation ("create a folder for X", "this is a
   project for Y", "Y is one of my projects, put it there"), set
   \`createsSubfolder: true\` and use the directed name. Otherwise file
   in 0-Inbox.

5. NEVER invent folder names not justified by the capture itself.
   Speculative creation is forbidden — when in doubt, 0-Inbox.

6. Top-level dirs always exist; you don't need createsSubfolder for
   them. The flag matters for sub-folders inside Projects/Areas/Resources.
   The bot validates and rejects ops that try to create a sub-folder
   without the flag set.

CONTENT:

7. Every CREATE body must start with YAML frontmatter:
   ---
   created: ${today}
   source: alfred-braindump
   tags: [free-form-list]
   ---

8. Read sibling notes in your chosen bucket and include [[wiki-links]]
   to thematically related ones. Don't fabricate links — only link
   real files you've read.

9. APPEND fragments don't need their own frontmatter (target already
   has one). Just the markdown body — the bot prepends a dated separator.

10. When you create multiple files in a NEW sub-folder, also create an
    index.md or README.md in that folder linking the siblings. This
    gives the user a single entry point.

CONFIDENCE:

11. Set confidence honestly. Below 0.6 means "I'm guessing on placement
    or decomposition" — for those captures, prefer the safe path
    (0-Inbox or append to a catch-all). Don't surface alternatives —
    the bot doesn't escalate; the user corrects via a follow-up
    capture if needed.

VAULT STRUCTURE:
${vaultSummary}`;
}

// ==================== VAULT SCAN ====================

/** Compact tree summary of the vault's top three levels. Mirrors the Rust
 *  summarize_vault() in chores/distiller/src/main.rs. */
function summarizeVault(vault: string): string {
  const out: string[] = [`${vault}/`];
  let tops: fs.Dirent[];
  try {
    tops = fs
      .readdirSync(vault, { withFileTypes: true })
      .filter((e) => e.isDirectory() && !e.name.startsWith("."))
      .sort((a, b) => a.name.localeCompare(b.name));
  } catch {
    return out.join("\n");
  }
  for (const top of tops) {
    out.push(`  ${top.name}/`);
    const topPath = path.join(vault, top.name);
    let subs: fs.Dirent[] = [];
    try {
      subs = fs
        .readdirSync(topPath, { withFileTypes: true })
        .sort((a, b) => a.name.localeCompare(b.name));
    } catch {
      continue;
    }
    const subDirs = subs.filter((e) => e.isDirectory());
    const subNotes = subs.filter(
      (e) => e.isFile() && e.name.endsWith(".md") && e.name !== "README.md",
    );
    for (const sub of subDirs.slice(0, 20)) {
      const noteCount = countNotes(path.join(topPath, sub.name));
      out.push(`    ${sub.name}/  (${noteCount} notes)`);
    }
    if (subDirs.length > 20) out.push(`    … and ${subDirs.length - 20} more sub-folders`);
    for (const note of subNotes.slice(0, 10)) {
      out.push(`    ${note.name}`);
    }
    if (subNotes.length > 10) out.push(`    … and ${subNotes.length - 10} more notes`);
  }
  return out.join("\n");
}

function countNotes(dir: string): number {
  try {
    return fs.readdirSync(dir).filter((n) => n.endsWith(".md")).length;
  } catch {
    return 0;
  }
}
