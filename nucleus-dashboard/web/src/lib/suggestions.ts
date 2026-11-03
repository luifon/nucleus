// Pure helpers for the skill suggestions on the Skills page (ADR-038):
// labels, the deep link, the list split, and the wording of confirmations
// and results. The server (nucleus_core::skill_learner) decides every
// action; nothing here predicts or repeats its rules. What the page knows
// about a refusal comes from the server: `accept_blockers` on the review,
// or the refusal of the action itself.

import type { StatusKind } from "@/components/StatusPill";
import type {
  EvidenceSource,
  LiveSkill,
  Suggestion,
  SuggestionAcceptResp,
  SuggestionKind,
  SuggestionOrigin,
  SuggestionStatus,
  SuggestionSummary,
} from "@/lib/api/skills";
import { gitSummary, type ResultSummary } from "@/lib/skills";

/** The query parameter of the deep link `/skills?suggestion=<id>`. */
export const SUGGESTION_PARAM = "suggestion";

/** The suggestion id in the page's query, or null when absent or not a
 *  positive integer. */
export function suggestionFromSearch(params: URLSearchParams): number | null {
  const raw = params.get(SUGGESTION_PARAM);
  if (raw === null || !/^[1-9][0-9]*$/.test(raw)) return null;
  const n = Number(raw);
  return Number.isSafeInteger(n) ? n : null;
}

/** `params` with the suggestion set (or removed when null); other
 *  parameters are kept. */
export function withSuggestion(params: URLSearchParams, id: number | null): URLSearchParams {
  const next = new URLSearchParams(params);
  if (id === null) next.delete(SUGGESTION_PARAM);
  else next.set(SUGGESTION_PARAM, String(id));
  return next;
}

/** Pending suggestions first, the decided ones (accepted, rejected,
 *  superseded) as history. Both keep the server's order (newest first). */
export function splitSuggestions(list: readonly SuggestionSummary[]): {
  pending: SuggestionSummary[];
  history: SuggestionSummary[];
} {
  return {
    pending: list.filter((s) => s.status === "pending"),
    history: list.filter((s) => s.status !== "pending"),
  };
}

export function kindLabel(kind: SuggestionKind): string {
  return kind === "create" ? "new skill" : "patch";
}

export function originLabel(origin: SuggestionOrigin): string {
  switch (origin) {
    case "live":
      return "live sweep";
    case "backfill":
      return "backfill";
    case "manual":
      return "filed by a session";
  }
}

export function statusKind(status: SuggestionStatus): StatusKind {
  switch (status) {
    case "pending":
      return "warn";
    case "accepted":
      return "ok";
    case "rejected":
      return "down";
    case "superseded":
      return "idle";
  }
}

export function evidenceSourceLabel(source: EvidenceSource): string {
  switch (source) {
    case "transcript":
      return "transcript";
    case "index":
      return "session index";
    case "manual":
      return "filed by hand";
  }
}

/** `text` on one line, cut to `max` characters with `…`. */
export function oneLine(text: string, max: number): string {
  const flat = text.split(/\s+/).filter(Boolean).join(" ");
  if ([...flat].length <= max) return flat;
  return [...flat].slice(0, Math.max(0, max - 1)).join("") + "…";
}

/** A date as dd/mm/yy (local time); the input when it does not parse. */
export function shortDate(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleDateString("en-GB", { day: "2-digit", month: "2-digit", year: "2-digit" });
}

/** A SKILL.md split into its YAML frontmatter (without the `---` lines)
 *  and the Markdown body. No frontmatter: `frontmatter` is null and the
 *  body is the whole text. */
export function splitFrontmatter(text: string): { frontmatter: string | null; body: string } {
  const t = text.replace(/\r\n/g, "\n");
  if (!t.startsWith("---\n")) return { frontmatter: null, body: text };
  const rest = t.slice(4);
  const end = rest.indexOf("\n---\n");
  if (end !== -1) return { frontmatter: rest.slice(0, end), body: rest.slice(end + 5) };
  if (rest.endsWith("\n---")) return { frontmatter: rest.slice(0, -4), body: "" };
  return { frontmatter: null, body: text };
}

/** Where accept writes, as the private tier documents it. */
export function skillFilePath(name: string): string {
  return `.nucleus/.claude/skills/${name}/SKILL.md`;
}

/** The sentence about the live skill a patch replaces. */
export function liveSkillNote(live: LiveSkill): { kind: StatusKind; text: string } {
  switch (live.state) {
    case "unchanged":
      return { kind: "ok", text: "The live skill has not changed since this suggestion was written." };
    case "changed":
      return {
        kind: "down",
        text: "The live skill changed after this suggestion was written. The changes below compare the draft with the live file as it is now.",
      };
    case "missing":
      return { kind: "down", text: `The live skill cannot be read${live.problem ? `: ${live.problem}` : "."}` };
  }
}

/** Text of the inline accept confirmation. */
export function acceptConfirmText(s: Pick<Suggestion, "kind" | "name">): string {
  return s.kind === "create"
    ? `Create the private skill ${s.name} at ${skillFilePath(s.name)}? It loads in the next Nucleus session.`
    : `Replace the private skill ${s.name} (${skillFilePath(s.name)}) with this draft? Refused if the live file changed since the suggestion.`;
}

/** Operator-facing summary of an accept. */
export function describeAccept(resp: SuggestionAcceptResp): ResultSummary {
  const git = gitSummary(resp.git);
  const lines = [`accepted #${resp.accepted.id}: wrote ${skillFilePath(resp.accepted.name)}`, ...git.lines];
  if (resp.accepted.superseded.length > 0) {
    lines.push(
      `superseded the other pending suggestions for ${resp.accepted.name}: ${resp.accepted.superseded.map((i) => `#${i}`).join(", ")}`,
    );
  }
  return { kind: git.kind, lines };
}

/** The first line of a refusal that lists draft problems: the server's
 *  text without the problems when it ends with them (they are listed
 *  below it), the whole text otherwise. */
export function refusalHead(message: string, issues: readonly string[]): string {
  const tail = `: ${issues.join("; ")}`;
  return issues.length > 0 && message.endsWith(tail) ? message.slice(0, -tail.length) : message;
}
