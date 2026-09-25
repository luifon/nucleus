// Every fixed text the WhatsApp bot sends (ADR-033, operator decision:
// English everywhere for code-owned texts). One place: the defaults below,
// each overridable in nucleus.toml under [whatsapp.texts] with the key in
// snake_case (`progress_prefix = "…"`), and the infrastructure reasons under
// [whatsapp.texts.infra_reasons]. Placeholders in braces are filled by the
// caller. The model writes everything else in the language of the chat.
//
// Task result status lines are not here: tasks are venue-agnostic and their
// lines live under [tasks.texts] (core/src/config.rs).

export interface BotTexts {
  // ── conversation (chat_engine.ts) ──
  /** The code-sent acknowledgement. */
  ack: string;
  /** Put before a progress message. */
  progressPrefix: string;
  interrupted: string;
  backgroundLost: string;
  sessionDied: string;
  noFinal: string;
  noAnswer: string;
  transcriptReset: string;
  /** {hours} */
  ceiling: string;
  /** {what}: one of infraReasons */
  infra: string;
  infraReasons: Record<string, string>;
  /** {error} */
  submitFailed: string;
  /** A message over the typed-prompt limit: {kib} {max} (KiB) */
  messageTooLong: string;
  /** {tmux} */
  permissionStall: string;
  /** {error} */
  transcriptionFailed: string;
  /** Appended when the secret filter redacted values: {count} */
  secretsWithheld: string;

  // ── documents and document jobs (ADR-013/018) ──
  /** {mb} {cap} */
  fileTooLarge: string;
  /** {error} */
  downloadFailed: string;
  /** {error} */
  archiveFailed: string;
  /** {name} {id} */
  archived: string;
  /** {name} {id} */
  archivedDuplicate: string;
  importStarted: string;
  /** {name} {error} */
  importFailed: string;
  /** {error} */
  importFailedInWindow: string;
  actStarted: string;
  /** {name} {reply} */
  actResult: string;
  /** {name} {error} */
  actFailed: string;
  /** {error} */
  actFailedInWindow: string;
  noResponse: string;
  /** {instruction} {id} */
  jobOrphaned: string;

  // ── brain dump (ADR-005a) ──
  received: string;
  /** {seconds} */
  transcribing: string;
  interpreting: string;
  applying: string;
  correcting: string;
  nothingToFile: string;
  notUnderstood: string;
  /** {id} */
  planCancelled: string;
  /** {id} */
  planSuperseded: string;
  /** {id} */
  planExpired: string;
  /** {error} */
  planFailed: string;
  /** {error} */
  interpretFailed: string;
  /** {error} */
  applyFailed: string;
}

export const DEFAULT_TEXTS: BotTexts = {
  ack: "⏳ Working on it…",
  progressPrefix: "↻ Progress: ",
  interrupted: "⚠️ Interrupted: the bot restarted before the reply. Not resumed — send it again if you still want it.",
  backgroundLost:
    "⚠️ The bot restarted while a background command was running; its result will not arrive. Send the request again if you still want it.",
  sessionDied: "⚠️ The session closed before replying. Send it again if you still want it.",
  noFinal: "⚠️ The session ended its turn without a reply.",
  noAnswer: "⚠️ No answer arrived within the safety limit. Send it again if you still want it.",
  transcriptReset:
    "⚠️ The session's transcript was reset, so this message's reply cannot be followed. Send it again if you still want it.",
  ceiling: "⚠️ This reply passed the {hours} h safety limit; I interrupted the session.",
  infra: "⚠️ I could not reply: {what}.",
  infraReasons: {
    "model-unavailable": "the model is unavailable",
    api: "the API is down or overloaded",
    "not-logged-in": "the claude CLI is not logged in",
    "usage-limit": "the account reached its usage limit",
    "no-turn": "the session produced no reply",
  },
  submitFailed: "⚠️ I could not deliver your message to the session: {error}",
  messageTooLong:
    "⚠️ That message is {kib} KiB; the session accepts at most {max} KiB of typed text. Send it in shorter messages, or as a document.",
  permissionStall: "⚠️ The session is waiting for a confirmation on screen (tmux attach -t {tmux}).",
  transcriptionFailed: "⚠️ I could not transcribe the voice memo: {error}",
  secretsWithheld: "({count} value(s) withheld by the secret filter)",

  fileTooLarge: "That file is about {mb} MB, over the {cap} MB library limit; not archived.",
  downloadFailed: "⚠️ I could not download that file: {error}",
  archiveFailed: "⚠️ I could not archive that file: {error}",
  archived: "📄 Archived: {name} (id {id})",
  archivedDuplicate: "📄 Archived: {name} (id {id}) — already in the library",
  importStarted: "Received; extracting into the vault. I will tell you when it is done 📥",
  importFailed: "📥 Import of \"{name}\" failed:\n```\n{error}\n```",
  importFailedInWindow: "Archived, but the import failed:\n```\n{error}\n```",
  actStarted: "Received; analyzing. The answer follows 📄",
  actResult: "📄 {name}:\n{reply}",
  actFailed: "📄 {name}: the analysis failed:\n```\n{error}\n```",
  actFailedInWindow: "Archived, but I could not process the request:\n```\n{error}\n```",
  noResponse: "(no response)",
  jobOrphaned: "⚠️ Job interrupted by a restart: {instruction} (job {id}). Send it again if you still want it.",

  received: "✓ received",
  transcribing: "🎧 transcribing a {seconds}s memo…",
  interpreting: "⚙️ interpreting…",
  applying: "📂 applying…",
  correcting: "✏️ correcting and applying…",
  nothingToFile: "✓ nothing to file",
  notUnderstood: "I did not understand; can you rephrase?",
  planCancelled: "✓ plan #{id} cancelled",
  planSuperseded: "⏱ plan #{id} cancelled — processing the new capture",
  planExpired: "⏱ plan #{id} expired — send it again if you still want it",
  planFailed: "⚠️ I could not plan that: {error}",
  interpretFailed: "⚠️ I could not interpret the reply: {error}",
  applyFailed: "⚠️ I could not apply the plan: {error}",
};

/** Replace `{name}` placeholders. Unknown placeholders stay as written. */
export function fill(template: string, vars: Record<string, string | number>): string {
  return template.replace(/\{(\w+)\}/g, (m, k) => (k in vars ? String(vars[k]) : m));
}

function camel(snake: string): string {
  return snake.replace(/_([a-z])/g, (_, c: string) => c.toUpperCase());
}

/** Defaults overlaid with a parsed [whatsapp.texts] table. Unknown keys and
 *  empty or non-string values are ignored. */
export function textsFrom(table: Record<string, unknown>): BotTexts {
  const out: BotTexts = { ...DEFAULT_TEXTS, infraReasons: { ...DEFAULT_TEXTS.infraReasons } };
  for (const [k, v] of Object.entries(table ?? {})) {
    if (k === "infra_reasons" && v && typeof v === "object") {
      for (const [rk, rv] of Object.entries(v as Record<string, unknown>)) {
        if (typeof rv === "string" && rv.trim()) out.infraReasons[rk.replace(/_/g, "-")] = rv;
      }
      continue;
    }
    const key = camel(k) as keyof BotTexts;
    if (key in DEFAULT_TEXTS && key !== "infraReasons" && typeof v === "string" && v.trim()) {
      (out as unknown as Record<string, string>)[key] = v;
    }
  }
  return out;
}
