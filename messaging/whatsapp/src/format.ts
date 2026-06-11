// Shared outbound-message formatting (ADR-020 dedup — this lived as two
// drifting copies in index.ts and ack.ts).
//
// Deliberately its own module rather than folded into ack.ts: ack.ts runs
// main() at module top level, so importing it from index.ts would fire a
// queue write on bot boot.

/** Format an outbound message so it's distinguishable from the user's own
 *  typed messages in the same self-group: bold body + persona signature.
 *  WhatsApp bold uses single asterisks and DOES NOT cross newlines
 *  reliably — wrap each non-empty line individually. */
export function formatReply(body: string, personaName: string): string {
  const bolded = body
    .split("\n")
    .map((line) => (line.trim() ? `*${line}*` : line))
    .join("\n");
  return `${bolded}\n\n*— ${personaName}*`;
}
