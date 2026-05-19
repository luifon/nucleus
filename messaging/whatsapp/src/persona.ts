import fs from "node:fs";
import path from "node:path";

/** Resolved persona ready for spawn-time use. Mirrors the Rust
 *  `nucleus_core::config::PersonaContent` shape. */
export interface PersonaContent {
  /** Markdown body (frontmatter stripped, `${USER_NAME}` substituted).
   *  Feed into `SpawnOptions.appendSystemPrompt`. */
  body: string;
  /** Human-readable name from the file's frontmatter `display_name`, or
   *  the slug if frontmatter is absent. Surfaced in reply footers etc. */
  displayName: string;
}

/** Resolve the persona for a conversational venue. See ADR-009.
 *
 *  Reads `NUCLEUS_PERSONA_<VENUE>` (and, if `context` is provided,
 *  `NUCLEUS_PERSONA_<VENUE>_<CONTEXT>` first — ADR-005b extension),
 *  loads `<workspaceRoot>/personas/<slug>.md`, parses optional YAML
 *  frontmatter for `display_name`, strips frontmatter from the body,
 *  applies `${USER_NAME}` substitution.
 *
 *  Missing env var or missing file is a hard error — no silent fallback,
 *  per ADR-009 §"Spawn-time resolution". */
export function resolvePersona(
  workspaceRoot: string,
  userName: string,
  venue: string,
  context?: string,
): PersonaContent {
  const venueUpper = venue.toUpperCase();
  let envKey: string;
  let slug: string | undefined;

  if (context) {
    const ctxUpper = context.toUpperCase();
    const scoped = `NUCLEUS_PERSONA_${venueUpper}_${ctxUpper}`;
    const scopedVal = process.env[scoped]?.trim();
    if (scopedVal) {
      envKey = scoped;
      slug = scopedVal;
    } else {
      const venueKey = `NUCLEUS_PERSONA_${venueUpper}`;
      const venueVal = process.env[venueKey]?.trim();
      if (!venueVal) {
        throw new Error(
          `neither \`${scoped}\` nor \`${venueKey}\` is set; one is required to ` +
            `resolve a persona for venue \`${venue}\` (context \`${context}\`)`,
        );
      }
      envKey = venueKey;
      slug = venueVal;
    }
  } else {
    envKey = `NUCLEUS_PERSONA_${venueUpper}`;
    slug = process.env[envKey]?.trim();
    if (!slug) {
      throw new Error(
        `required env var \`${envKey}\` is not set; define a persona slug for ` +
          `venue \`${venue}\` in .env (see ADR-009)`,
      );
    }
  }

  const filePath = path.join(workspaceRoot, "personas", `${slug}.md`);
  if (!fs.existsSync(filePath)) {
    throw new Error(
      `persona file ${filePath} not found (resolved from ${envKey}=${slug})`,
    );
  }
  const raw = fs.readFileSync(filePath, "utf-8");

  const { frontmatter, body: rawBody } = splitFrontmatter(raw);
  const displayName = frontmatter
    ? (extractYamlField(frontmatter, "display_name") ?? slug)
    : slug;
  const body = rawBody.replace(/\$\{USER_NAME\}/g, userName);

  return { body, displayName };
}

interface FrontmatterSplit {
  frontmatter: string | null;
  body: string;
}

/** Splits a YAML frontmatter block off the start of a markdown string.
 *  Returns `{ frontmatter, body }`; frontmatter is null when the document
 *  doesn't open with `---\n...\n---\n`. */
function splitFrontmatter(s: string): FrontmatterSplit {
  const stripped = s.replace(/^﻿/, "");
  const openMatch = stripped.match(/^---\r?\n/);
  if (!openMatch) return { frontmatter: null, body: s };
  const afterOpen = stripped.slice(openMatch[0].length);
  let searchFrom = 0;
  while (true) {
    const idx = afterOpen.indexOf("\n---", searchFrom);
    if (idx < 0) return { frontmatter: null, body: s };
    const after = afterOpen.slice(idx + 4);
    if (after.length === 0 || after.startsWith("\n") || after.startsWith("\r\n")) {
      const frontmatter = afterOpen.slice(0, idx);
      const body = after.replace(/^\r?\n/, "");
      return { frontmatter, body };
    }
    searchFrom = idx + 4;
  }
}

/** Pulls a single scalar field out of a tiny YAML frontmatter — just the
 *  shapes we ship (`display_name: foo`, with optional quotes). Not a full
 *  YAML parser; the frontmatter contract is intentionally narrow. */
function extractYamlField(frontmatter: string, field: string): string | undefined {
  for (const rawLine of frontmatter.split(/\r?\n/)) {
    const line = rawLine.trim();
    if (!line || line.startsWith("#")) continue;
    const eq = line.indexOf(":");
    if (eq < 0) continue;
    if (line.slice(0, eq).trim() !== field) continue;
    let value = line.slice(eq + 1).trim();
    if (value.length >= 2) {
      const first = value[0];
      const last = value[value.length - 1];
      if ((first === '"' && last === '"') || (first === "'" && last === "'")) {
        value = value.slice(1, -1);
      }
    }
    return value.length > 0 ? value : undefined;
  }
  return undefined;
}
