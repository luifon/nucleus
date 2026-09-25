// Runtime secret filter for outbound WhatsApp text (ADR-033 §4).
//
// Every row the outbound drain sends (conversation replies, progress
// messages, acknowledgements, task results, reminders, job replies, media
// captions) passes through `filterOutbound` first. The rules are the ones
// tools/check-secrets.sh applies to commits, read at runtime from the same
// sources:
//
//   A. `.env` values (>= 6 characters, minus benign values), plus the
//      operator's home directory;
//   B. the gitignored `.claude/secret-strings` denylist (whole word,
//      case-insensitive);
//   C. PII patterns: email addresses, WhatsApp JIDs, E.164 phone numbers,
//      home-directory paths.
//
// Plus credential-shaped tokens (private keys, API tokens) that should never
// leave the machine in any message.
//
// How a match is handled depends on the message:
//
//   - Credentials — `.env` values whose key names a credential (TOKEN,
//     SECRET, KEY, PASSWORD, …) and credential-shaped tokens — are redacted
//     in every message.
//   - A progress message (model narration sent while a turn runs) with any
//     match from A, B or C is withheld entirely: progress is optional, and
//     narration never needs an identifier.
//   - A message to a group is shared with its members: matches from A, B and
//     C are redacted.
//   - A final reply, note or result in the operator's own DM keeps the other
//     values: the operator asked for them (a stand-up summary names clients,
//     a lookup returns a phone number), and they are the operator's data.
//
// A redaction replaces the value with "[redacted]" and appends a note with
// the count. The drain logs every hit by kind and count, never the value.

import fs from "node:fs";
import path from "node:path";

export type Audience = "operator-dm" | "shared";

export interface SecretRules {
  /** Redacted everywhere. */
  credentials: string[];
  /** .env identifier values and the home directory (substring match). */
  identifiers: string[];
  /** Denylist literals (whole word, case-insensitive). */
  words: string[];
}

export interface FilterResult {
  text: string;
  /** One entry per match: its kind (never the value). */
  hits: string[];
  /** True when the whole message must not be sent. */
  withheld: boolean;
}

const BENIGN_VALUES = new Set(["info", "debug", "warn", "error", "trace", "true", "false", "claude"]);
const CREDENTIAL_KEY = /(TOKEN|SECRET|PASSWORD|PASSWD|CREDENTIAL|PRIVATE|COOKIE|(^|_)KEY($|_)|API_?KEY|AUTH)/i;
/** Keys whose values are configuration, not identifiers. */
const CONFIG_KEY = /(^|_)(TZ|LOG|LEVEL|MODEL|PORT|BIN|PATH|MODE|INTERVAL|TIMEOUT|DELAY|SECONDS|MS)($|_)/i;

/** Placeholders tools/check-secrets.sh also ignores. */
const PLACEHOLDERS = /example\.(com|org|net)|5511999999999|you@|@example|\/path\/to\//i;

const PII: Array<[RegExp, string]> = [
  [/[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}/g, "pii-email"],
  [/\+?[0-9]{6,}(?::[0-9]+)?@(?:s\.whatsapp\.net|g\.us|c\.us|lid)/g, "pii-whatsapp-jid"],
  [/(?:\+55|\+1)[0-9]{9,}/g, "pii-phone-e164"],
  [/\/(?:Users|home)\/[a-z][a-z0-9_-]{2,}\//g, "pii-home-path"],
];

/** How a credential shape's match is redacted: the whole match, or group 2
 *  after a kept label in group 1 (`random`: only when group 2 looks random). */
type Part = "whole" | "value" | "random";

/** Credential-shaped tokens, most specific first. Mirrors `shapes()` in
 *  core/src/secret_filter.rs; both run core/testdata/credential_vectors.json. */
const CREDENTIAL_SHAPES: Array<[RegExp, string, Part]> = [
  [/-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----[\s\S]*?(?:-----END [A-Z0-9 ]*PRIVATE KEY-----|$)/g, "credential-private-key", "whole"],
  [/\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}/g, "credential-jwt", "whole"],
  [/\bsk-(?:ant-)?[A-Za-z0-9_-]{20,}/g, "credential-api-key", "whole"],
  [/\b[sr]k_(?:live|test)_[A-Za-z0-9]{16,}/g, "credential-stripe", "whole"],
  [/\bAIza[0-9A-Za-z_-]{35}/g, "credential-google", "whole"],
  [/\bglpat-[A-Za-z0-9_-]{20,}/g, "credential-gitlab", "whole"],
  [/\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{30,}/g, "credential-github", "whole"],
  [/\bgithub_pat_[A-Za-z0-9_]{30,}/g, "credential-github", "whole"],
  [/\bxox[abeoprs]-[A-Za-z0-9-]{10,}/g, "credential-slack", "whole"],
  [/\b(?:AKIA|ASIA)[0-9A-Z]{16}\b/g, "credential-aws", "whole"],
  [/(aws[A-Za-z0-9_ -]{0,20}secret[A-Za-z0-9_ -]{0,20}["']?\s*[:=]\s*["']?)([A-Za-z0-9/+=]{40})/gi, "credential-aws", "value"],
  [/\bBearer\s+[A-Za-z0-9._~+/=-]{20,}/g, "credential-bearer", "whole"],
  [
    /((?:\b|_)(?:password|passwd|pwd|passphrase|secret|client[_-]?secret|token|access[_-]?token|refresh[_-]?token|auth[_-]?token|api[_-]?key|apikey|access[_-]?key|secret[_-]?key|private[_-]?key)\b["']?\s*[:=]\s*["']?)([^\s"'<>,;]{8,})/gi,
    "credential-labeled",
    "random",
  ],
];

/** A labeled value counts as a secret when it has at least 8 characters,
 *  mixes at least two character classes, and has a Shannon entropy of at
 *  least 3 bits per character. Mirrors `looks_random` in Rust. */
export function looksRandom(v: string): boolean {
  const chars = Array.from(v);
  if (chars.length < 8) return false;
  const classes = [/[a-z]/, /[A-Z]/, /[0-9]/, /[^A-Za-z0-9]/].filter((re) => re.test(v)).length;
  if (classes < 2) return false;
  const counts = new Map<string, number>();
  for (const c of chars) counts.set(c, (counts.get(c) ?? 0) + 1);
  let h = 0;
  for (const k of counts.values()) {
    const p = k / chars.length;
    h -= p * Math.log2(p);
  }
  return h >= 3.0;
}

function redactShape(text: string, re: RegExp, kind: string, part: Part, hits: string[]): string {
  return text.replace(re, (m: string, g1?: string, g2?: string) => {
    if (part === "whole") {
      hits.push(kind);
      return REDACTED;
    }
    if (part === "random" && !looksRandom(g2 ?? "")) return m;
    hits.push(kind);
    return `${g1 ?? ""}${REDACTED}`;
  });
}

/** Redact credentials only (the `.env` credential values and the shapes).
 *  Pure. */
export function redactCredentials(text: string, rules: Pick<SecretRules, "credentials">): { text: string; hits: string[] } {
  const hits: string[] = [];
  let out = text;
  for (const v of rules.credentials) out = replaceLiteral(out, v, "env-credential", hits);
  for (const [re, kind, part] of CREDENTIAL_SHAPES) out = redactShape(out, re, kind, part, hits);
  return { text: out, hits };
}

function unquote(v: string): string {
  const t = v.trim();
  if ((t.startsWith('"') && t.endsWith('"')) || (t.startsWith("'") && t.endsWith("'"))) {
    return t.slice(1, -1);
  }
  return t;
}

/** Rules from `.env` text, denylist text and the home directory. Pure. */
export function buildRules(envText: string, denylistText: string, home: string): SecretRules {
  const credentials: string[] = [];
  const identifiers: string[] = [];
  for (const raw of envText.split("\n")) {
    const line = raw.trim();
    if (!line || line.startsWith("#")) continue;
    const eq = line.indexOf("=");
    if (eq < 0) continue;
    const key = line.slice(0, eq).replace(/^export\s+/, "").trim();
    const value = unquote(line.slice(eq + 1));
    for (const part of value.split(",").map((p) => p.trim())) {
      if (part.length < 6 || BENIGN_VALUES.has(part.toLowerCase())) continue;
      if (CREDENTIAL_KEY.test(key)) credentials.push(part);
      else if (!CONFIG_KEY.test(key)) identifiers.push(part);
    }
  }
  if (home.length >= 6) identifiers.push(home);
  const words = denylistText
    .split("\n")
    .map((l) => l.trim())
    .filter((l) => l && !l.startsWith("#"));
  // Longest first, so a value that contains another is redacted whole.
  const byLen = (a: string, b: string) => b.length - a.length;
  return { credentials: credentials.sort(byLen), identifiers: identifiers.sort(byLen), words };
}

function escapeRe(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

const REDACTED = "[redacted]";

function replaceLiteral(text: string, value: string, kind: string, hits: string[]): string {
  if (!value || !text.includes(value)) return text;
  const parts = text.split(value);
  for (let i = 1; i < parts.length; i++) hits.push(kind);
  return parts.join(REDACTED);
}

function replaceRe(text: string, re: RegExp, kind: string, hits: string[], skipPlaceholders: boolean): string {
  return text.replace(re, (m) => {
    if (skipPlaceholders && PLACEHOLDERS.test(m)) return m;
    hits.push(kind);
    return REDACTED;
  });
}

/** Apply the rules to one outbound text. Pure. */
export function filterOutbound(
  text: string,
  rules: SecretRules,
  opts: { audience: Audience; progress: boolean; note?: string },
): FilterResult {
  // Credentials: every message.
  const cred = redactCredentials(text, rules);
  const hits: string[] = cred.hits;
  let out = cred.text;

  const strict = opts.progress || opts.audience === "shared";
  if (strict) {
    for (const v of rules.identifiers) out = replaceLiteral(out, v, "env-value", hits);
    for (const w of rules.words) {
      const re = new RegExp(`(?<![\\p{L}\\p{N}_])${escapeRe(w)}(?![\\p{L}\\p{N}_])`, "giu");
      out = replaceRe(out, re, "denylist", hits, false);
    }
    for (const [re, kind] of PII) out = replaceRe(out, re, kind, hits, true);
  }

  if (opts.progress && hits.length > 0) return { text: "", hits, withheld: true };
  if (hits.length > 0) {
    const note = opts.note ?? "({count} value(s) withheld by the secret filter)";
    out += `\n\n${note.replace("{count}", String(hits.length))}`;
  }
  return { text: out, hits, withheld: false };
}

/** Rules loaded from the workspace, re-read when a source file changes. */
export class SecretRuleSource {
  private rules: SecretRules = { credentials: [], identifiers: [], words: [] };
  private stamp = "";

  constructor(
    private readonly workspaceRoot: string,
    private readonly home: string = process.env.HOME ?? "",
  ) {}

  private files(): string[] {
    return [path.join(this.workspaceRoot, ".env"), path.join(this.workspaceRoot, ".claude/secret-strings")];
  }

  current(): SecretRules {
    const stamp = this.files()
      .map((f) => {
        try {
          const st = fs.statSync(f);
          return `${f}:${st.mtimeMs}:${st.size}`;
        } catch {
          return `${f}:-`;
        }
      })
      .join("|");
    if (stamp !== this.stamp) {
      const read = (f: string) => {
        try {
          return fs.readFileSync(f, "utf8");
        } catch {
          return "";
        }
      };
      const [env, deny] = this.files().map(read);
      this.rules = buildRules(env, deny, this.home);
      this.stamp = stamp;
    }
    return this.rules;
  }
}
