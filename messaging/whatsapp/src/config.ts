import fs from "node:fs";
import path from "node:path";

/** Minimal TOML reader — supports flat tables, scalars, single-line AND
 * multi-line string arrays. Doesn't handle nested tables, inline tables,
 * dotted keys, etc. Fine for our config surface. */
function parseToml(src: string): Record<string, any> {
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
    const tableMatch = line.match(/^\[([^\]]+)\]$/);
    if (tableMatch) {
      const name = tableMatch[1];
      table = out[name] = (out[name] as Record<string, any>) ?? {};
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

function substitute(s: string): string {
  return s.replace(/\$\{USER_NAME\}/g, process.env.NUCLEUS_USER_NAME ?? "user");
}

export interface Config {
  workspaceRoot: string;
  userName: string;
  claudeBin: string;
  permissionMode: string;
  disallowedTools: string[];
  /** Conversational allowlist (Alfred persona): JID → role "alfred". */
  allowedChatIds: string[];
  /** Conversational allowlist by group name. */
  allowedGroupNames: string[];
  /** Brain-dump capture allowlist: JID → role "braindump". */
  brainDumpChatIds: string[];
  /** Brain-dump capture allowlist by group name. */
  brainDumpGroupNames: string[];
  discoverMode: boolean;
  appendSystemPrompt: string;
  vaultPath: string;
  diaryRoot: string;
  dbPath: string;
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

  const personaPath = path.join(workspaceRoot, "messaging/whatsapp/persona.md");
  const persona = fs.existsSync(personaPath)
    ? substitute(fs.readFileSync(personaPath, "utf-8"))
    : "";

  const rawVault = (obsidian.vault_path ?? "~/Documents/Obsidian") as string;
  const vaultPath = rawVault.startsWith("~/")
    ? path.join(process.env.HOME ?? "", rawVault.slice(2))
    : rawVault;

  return {
    workspaceRoot,
    userName: envRequired("NUCLEUS_USER_NAME"),
    claudeBin: process.env.NUCLEUS_CLAUDE_BIN ?? claude.binary ?? "claude",
    permissionMode: claude.permission_mode ?? "auto",
    disallowedTools: claude.disallowed_tools ?? [],
    allowedChatIds: splitCsv(process.env.WHATSAPP_ALLOWED_CHAT_IDS),
    allowedGroupNames: splitCsv(process.env.WHATSAPP_ALLOWED_GROUP_NAMES),
    brainDumpChatIds: splitCsv(process.env.WHATSAPP_BRAINDUMP_CHAT_IDS),
    brainDumpGroupNames: splitCsv(process.env.WHATSAPP_BRAINDUMP_GROUP_NAMES),
    discoverMode: discover,
    appendSystemPrompt: persona,
    vaultPath,
    diaryRoot: path.resolve(workspaceRoot, diary.root ?? "memory/diaries"),
    dbPath: path.join(workspaceRoot, "memory/whatsapp.db"),
  };
}
