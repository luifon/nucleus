import { DEFAULT_BREAKER } from "./breaker.js";
import fs from "node:fs";
import path from "node:path";
import { resolvePersona } from "./persona.js";
import { textsFrom } from "./texts.js";
import { intakeConfig, type IntakeWhatsAppConfig } from "./intake.js";

/** Minimal TOML reader — supports tables (dotted names nest), arrays of
 * tables (`[[a.b]]`), scalars, single-line AND multi-line string arrays.
 * Doesn't handle inline tables or dotted keys. Fine for our config surface. */
export function parseToml(src: string): Record<string, any> {
  const out: Record<string, any> = {};
  let table: Record<string, any> = out;

  // First, glue multi-line arrays back onto a single logical line.
  const raw = src.split("\n");
  const lines: string[] = [];
  let buf: string | null = null;
  let depth = 0;
  for (const r of raw) {
    // Strip line comments only outside an array (commas inside strings are fine
    // because our values are simple).
    const stripped: string = buf === null ? r.replace(/#.*$/, "") : r;
    if (buf === null) {
      const eq = stripped.indexOf("=");
      const rhs = eq >= 0 ? stripped.slice(eq + 1).trim() : "";
      // Count opens vs closes on the RHS to decide if the array is multi-line.
      const opens = (rhs.match(/\[/g) ?? []).length;
      const closes = (rhs.match(/\]/g) ?? []).length;
      if (opens > closes) {
        buf = stripped;
        depth = opens - closes;
      } else {
        lines.push(stripped);
      }
    } else {
      buf += " " + stripped.trim();
      depth += (stripped.match(/\[/g) ?? []).length;
      depth -= (stripped.match(/\]/g) ?? []).length;
      if (depth <= 0) {
        lines.push(buf);
        buf = null;
        depth = 0;
      }
    }
  }
  if (buf !== null) lines.push(buf); // unterminated; let parseValue cope

  for (let line of lines) {
    line = line.trim();
    if (!line) continue;
    // [[a.b]] appends a new table to the array a.b ([[intake.repos]]).
    const arrayMatch = line.match(/^\[\[([^\]]+)\]\]$/);
    if (arrayMatch) {
      const parts = arrayMatch[1].split(".").map((p) => p.trim());
      let parent: Record<string, any> = out;
      for (const part of parts.slice(0, -1)) {
        parent = parent[part] = (parent[part] as Record<string, any>) ?? {};
      }
      const last = parts[parts.length - 1];
      const arr: Record<string, any>[] = Array.isArray(parent[last]) ? parent[last] : (parent[last] = []);
      table = {};
      arr.push(table);
      continue;
    }
    const tableMatch = line.match(/^\[([^\]]+)\]$/);
    if (tableMatch) {
      // Dotted names nest: [whatsapp.turns] → out.whatsapp.turns. (Before
      // this, [whatsapp.breaker] landed under the literal key
      // "whatsapp.breaker" and the breaker overrides were never applied.)
      table = out;
      for (const part of tableMatch[1].split(".").map((p) => p.trim())) {
        table = table[part] = (table[part] as Record<string, any>) ?? {};
      }
      continue;
    }
    const eq = line.indexOf("=");
    if (eq < 0) continue;
    const key = line.slice(0, eq).trim();
    const rhs = line.slice(eq + 1).trim();
    table[key] = parseValue(rhs);
  }
  return out;
}

function parseValue(raw: string): any {
  if (raw.startsWith("[") && raw.endsWith("]")) {
    const inner = raw.slice(1, -1).trim();
    if (!inner) return [];
    return inner.split(",").map((s) => parseValue(s.trim()));
  }
  if (raw.startsWith('"') && raw.endsWith('"')) return raw.slice(1, -1);
  if (raw === "true") return true;
  if (raw === "false") return false;
  if (/^-?\d+$/.test(raw)) return parseInt(raw, 10);
  if (/^-?\d+\.\d+$/.test(raw)) return parseFloat(raw);
  return raw;
}

function loadDotEnv(workspaceRoot: string): void {
  const p = path.join(workspaceRoot, ".env");
  if (!fs.existsSync(p)) return;
  for (const line of fs.readFileSync(p, "utf-8").split("\n")) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const eq = trimmed.indexOf("=");
    if (eq < 0) continue;
    const key = trimmed.slice(0, eq).trim();
    let value = trimmed.slice(eq + 1).trim();
    // Strip surrounding "double" or 'single' quotes — needed for values
    // with spaces (e.g., WHATSAPP_BRAINDUMP_GROUP_NAMES="Brain Dump"),
    // which bash also requires quoted to source the file. Without this,
    // the value would arrive in TS as the literal string with quotes.
    if (value.length >= 2) {
      const first = value[0];
      const last = value[value.length - 1];
      if ((first === '"' && last === '"') || (first === "'" && last === "'")) {
        value = value.slice(1, -1);
      }
    }
    if (process.env[key] === undefined) process.env[key] = value;
  }
}

function envRequired(key: string): string {
  const v = process.env[key];
  if (!v) throw new Error(`required env var ${key} is not set (see .env.example)`);
  return v;
}

function splitCsv(s: string | undefined): string[] {
  if (!s) return [];
  return s
    .split(",")
    .map((p) => p.trim())
    .filter((p) => p.length > 0);
}

/** Normalize a sender identifier (phone number or LID) to a digit-only
 *  string for set membership. Strips `@s.whatsapp.net`, `@lid`, leading
 *  `+`, spaces, hyphens, parentheses. Returns the user part only. */
export function normalizeSenderId(raw: string): string {
  const at = raw.indexOf("@");
  const head = at >= 0 ? raw.slice(0, at) : raw;
  // Drop any device suffix like `:2` so the env value matches no matter
  // which device session a message arrives under.
  const colon = head.indexOf(":");
  const userPart = colon >= 0 ? head.slice(0, colon) : head;
  return userPart.replace(/\D+/g, "");
}

export interface Config {
  workspaceRoot: string;
  userName: string;
  claudeBin: string;
  permissionMode: string;
  disallowedTools: string[];
  /** Conversational allowlist: JID → role "whatsapp-group". */
  allowedChatIds: string[];
  /** Conversational allowlist by group name. */
  allowedGroupNames: string[];
  /** Brain-dump capture allowlist: JID → role "braindump". */
  brainDumpChatIds: string[];
  /** Brain-dump capture allowlist by group name. */
  brainDumpGroupNames: string[];
  /** DM allowlist (ADR-005b): normalized digit-only user parts of JIDs
   *  the bot accepts DMs from. Empty = DM listening disabled. Accepts any
   *  shape in the env var (full `@s.whatsapp.net` JID, full `@lid` JID,
   *  bare phone number, bare LID) — same posture as `allowedSenders` for
   *  the group path. Match happens after normalizing the inbound DM's
   *  chatId user-part to digits. */
  allowedDmSenders: Set<string>;
  /** The operator: the first WHATSAPP_ALLOWED_DM_JIDS entry, normalized to
   *  digits (ADR-005b). The only identity whose issue-pipeline commands
   *  count (ADR-036); null when the list is empty. */
  operatorId: string | null;
  /** Per-sender authorization: only messages whose participant matches
   *  one of these IDs are processed, even inside an allowlisted group.
   *  Holds normalized digit-only user parts; comparison is against the
   *  participant's LID user part and (after `getPNForLID` resolution)
   *  the underlying phone number user part. */
  allowedSenders: Set<string>;
  discoverMode: boolean;
  /** Persona markdown body for conversational groups (ADR-005b: `group`
   *  context). Resolved via NUCLEUS_PERSONA_WHATSAPP_GROUP, falling back
   *  to NUCLEUS_PERSONA_WHATSAPP. Fed to `--append-system-prompt`. */
  appendSystemPromptGroup: string;
  /** Persona markdown body for DMs (ADR-005b: `dm` context). Resolved via
   *  NUCLEUS_PERSONA_WHATSAPP_DM, falling back to NUCLEUS_PERSONA_WHATSAPP. */
  appendSystemPromptDm: string;
  /** Persona markdown body for brain-dump spawns (ADR-005b: `braindump`
   *  context). Resolved via NUCLEUS_PERSONA_WHATSAPP_BRAINDUMP, falling
   *  back to NUCLEUS_PERSONA_WHATSAPP. */
  appendSystemPromptBraindump: string;
  /** Persona display name for the reply-signature footer. ADR-005b
   *  resolves persona *bodies* per context but keeps a single venue-level
   *  display name — the footer label is uniform across contexts. */
  personaDisplayName: string;
  vaultPath: string;
  diaryRoot: string;
  dbPath: string;
  /** ADR-017: on-the-fly skill-review nudge interval (asks per chat). 0 = off. */
  skillNudgeInterval: number;
  /** ADR-018: outbound media size cap (bytes). WHATSAPP_MEDIA_MAX_BYTES. */
  mediaMaxBytes: number;
  /** ADR-018: document-library binaries dir. WHATSAPP_DOCUMENTS_DIR override
   *  is the future external-drive/self-hosted-mirror seam; default
   *  <workspace>/memory/documents. */
  documentsDir: string;
  /** ADR-018: document-library metadata DB (fixed; TS-owned per ADR-020). */
  documentsDbPath: string;
  /** ADR-018: drain-owned staging dir for outbound media copies (fixed). */
  outboundStagingDir: string;
  /** ADR-013: jobs ledger DB (fixed; whatsapp-family-owned per ADR-020). */
  jobsDbPath: string;
  /** ADR-027 connection breaker knobs ([whatsapp.breaker] in nucleus.toml). */
  breaker: import("./breaker.js").BreakerConfig;
  /** ADR-033 turn engine knobs ([whatsapp.turns] in nucleus.toml). */
  turns: import("./chat_engine.js").TurnsConfig;
  /** ADR-027 amendment: link knobs ([whatsapp.link] in nucleus.toml). */
  link: LinkConfig;
  /** ADR-033: the `nucleus` binary (tasks CLI, skill review). */
  nucleusBin: string | null;
  /** ADR-036: issue-pipeline groups ([intake.whatsapp] in nucleus.toml). */
  intake: IntakeWhatsAppConfig;
}

export type { Config as default };

export function loadConfig(workspaceRoot: string, discover: boolean): Config {
  loadDotEnv(workspaceRoot);

  const tomlPath = path.join(workspaceRoot, "nucleus.toml");
  let parsed: Record<string, any> = {};
  if (fs.existsSync(tomlPath)) {
    parsed = parseToml(fs.readFileSync(tomlPath, "utf-8"));
  }
  const claude = parsed.claude ?? {};
  const diary = parsed.diary ?? { root: "memory/diaries" };
  const obsidian = parsed.obsidian ?? {};
  const skillLearner = parsed.skill_learner ?? {};

  const userName = envRequired("NUCLEUS_USER_NAME");
  const personaDefault = resolvePersona(workspaceRoot, userName, "whatsapp");
  const personaGroup = resolvePersona(workspaceRoot, userName, "whatsapp", "group");
  const personaDm = resolvePersona(workspaceRoot, userName, "whatsapp", "dm");
  const personaBraindump = resolvePersona(workspaceRoot, userName, "whatsapp", "braindump");

  const rawVault = (obsidian.vault_path ?? "~/Documents/Obsidian") as string;
  const vaultPath = rawVault.startsWith("~/")
    ? path.join(process.env.HOME ?? "", rawVault.slice(2))
    : rawVault;

  return {
    workspaceRoot,
    userName,
    claudeBin: process.env.NUCLEUS_CLAUDE_BIN ?? claude.binary ?? "claude",
    permissionMode: claude.permission_mode ?? "auto",
    disallowedTools: claude.disallowed_tools ?? [],
    allowedChatIds: splitCsv(process.env.WHATSAPP_ALLOWED_CHAT_IDS),
    allowedGroupNames: splitCsv(process.env.WHATSAPP_ALLOWED_GROUP_NAMES),
    brainDumpChatIds: splitCsv(process.env.WHATSAPP_BRAINDUMP_CHAT_IDS),
    brainDumpGroupNames: splitCsv(process.env.WHATSAPP_BRAINDUMP_GROUP_NAMES),
    allowedDmSenders: new Set(
      splitCsv(process.env.WHATSAPP_ALLOWED_DM_JIDS)
        .map(normalizeSenderId)
        .filter((s) => s.length > 0),
    ),
    operatorId: splitCsv(process.env.WHATSAPP_ALLOWED_DM_JIDS).map(normalizeSenderId).find((s) => s.length > 0) ?? null,
    allowedSenders: new Set(
      splitCsv(process.env.WHATSAPP_ALLOWED_SENDERS)
        .map(normalizeSenderId)
        .filter((s) => s.length > 0),
    ),
    discoverMode: discover,
    appendSystemPromptGroup: personaGroup.body,
    appendSystemPromptDm: personaDm.body,
    appendSystemPromptBraindump: personaBraindump.body,
    personaDisplayName: personaDefault.displayName,
    vaultPath,
    diaryRoot: path.resolve(workspaceRoot, diary.root ?? "memory/diaries"),
    dbPath: path.join(workspaceRoot, "memory/whatsapp.db"),
    // ADR-017 on-the-fly skill review: 0 disables (enabled=false in toml).
    skillNudgeInterval:
      skillLearner.enabled === false ? 0 : Number(skillLearner.nudge_interval ?? 12),
    // ADR-018 document library + outbound media.
    mediaMaxBytes: Number(process.env.WHATSAPP_MEDIA_MAX_BYTES ?? 64 * 1024 * 1024),
    documentsDir: process.env.WHATSAPP_DOCUMENTS_DIR
      ? path.resolve(process.env.WHATSAPP_DOCUMENTS_DIR)
      : path.join(workspaceRoot, "memory/documents"),
    documentsDbPath: path.join(workspaceRoot, "memory/documents.db"),
    outboundStagingDir: path.join(workspaceRoot, "memory/outbound-staging"),
    jobsDbPath: path.join(workspaceRoot, "memory/jobs.db"),
    breaker: breakerConfig(parsed.whatsapp?.breaker ?? {}),
    turns: turnsConfig(parsed.whatsapp?.turns ?? {}, parsed.whatsapp?.texts ?? {}),
    link: linkConfig(parsed.whatsapp?.link ?? {}),
    nucleusBin: findNucleusBin(workspaceRoot),
    intake: intakeConfig(parsed.intake?.whatsapp ?? {}),
  };
}

/** The built `nucleus` binary: release first, then debug; null when neither
 *  exists (a checkout that was never built). */
export function findNucleusBin(workspaceRoot: string): string | null {
  for (const p of ["target/release/nucleus", "target/debug/nucleus"]) {
    const full = path.join(workspaceRoot, p);
    if (fs.existsSync(full)) return full;
  }
  return null;
}

/** ADR-033: [whatsapp.turns] overrides layered over the defaults, plus the
 *  fixed texts from [whatsapp.texts] (texts.ts). */
export function turnsConfig(
  t: Record<string, unknown>,
  texts: Record<string, unknown> = {},
): import("./chat_engine.js").TurnsConfig {
  const pos = (v: unknown): number | null =>
    typeof v === "number" && Number.isFinite(v) && v > 0 ? v : null;
  return {
    ackAfterMs: (pos(t.ack_after_secs) ?? 30) * 1000,
    progressIntervalMs: (pos(t.progress_interval_secs) ?? 180) * 1000,
    progressMaxChars: pos(t.progress_max_chars) ?? 160,
    ceilingMs: (pos(t.turn_ceiling_hours) ?? 6) * 3_600_000,
    permissionStallMs: (pos(t.permission_stall_secs) ?? 120) * 1000,
    texts: textsFrom(texts),
  };
}

/** ADR-027 amendment: sent-message retention for retry requests and the WA
 *  Web version cache age. */
export interface LinkConfig {
  /** sent_messages retention; raised to the upstream resend window if lower. */
  sentRetentionMs: number;
  sentMaxRows: number;
  /** A cached WA Web version older than this is not used. */
  waVersionMaxAgeMs: number;
}

/** [whatsapp.link] overrides layered over the defaults. */
export function linkConfig(t: Record<string, unknown>): LinkConfig {
  const pos = (v: unknown): number | null =>
    typeof v === "number" && Number.isFinite(v) && v > 0 ? v : null;
  return {
    sentRetentionMs: (pos(t.sent_retention_days) ?? 21) * 24 * 3_600_000,
    sentMaxRows: pos(t.sent_max_rows) ?? 50_000,
    waVersionMaxAgeMs: (pos(t.wa_version_max_age_hours) ?? 168) * 3_600_000,
  };
}

/** ADR-027: [whatsapp.breaker] overrides layered over the defaults. */
function breakerConfig(t: Record<string, unknown>): import("./breaker.js").BreakerConfig {
  const d = DEFAULT_BREAKER;
  const num = (v: unknown, fallback: number) =>
    typeof v === "number" && Number.isFinite(v) && v > 0 ? v : fallback;
  return {
    ladderMs: Array.isArray(t.ladder_ms) && t.ladder_ms.every((x) => typeof x === "number")
      ? (t.ladder_ms as number[])
      : d.ladderMs,
    stableMs: num(t.stable_ms, d.stableMs),
    openThreshold: num(t.open_threshold, d.openThreshold),
    windowMs: num(t.window_ms, d.windowMs),
    probeMs: num(t.probe_ms, d.probeMs),
    alertAfterOutageMs: num(t.alert_after_outage_ms, d.alertAfterOutageMs),
  };
}
