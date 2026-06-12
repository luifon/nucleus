// ADR-018 inbound-media caption heuristics — separate module so tests
// can import it without executing index.ts's top-level main().

/** ADR-018 act-vs-archive decision for an inbound media caption. */
export function classifyCaption(caption: string): {
  mode: "archive" | "act";
  name?: string;
  instruction?: string;
} {
  const c = caption.trim();
  // Escape hatches first.
  const nameMatch = c.match(/^(?:name|nome):\s*(.+)$/is);
  if (nameMatch) return { mode: "archive", name: nameMatch[1].trim() };
  const actMatch = c.match(/^(?:act|faz):\s*(.+)$/is) ?? c.match(/^!\s*(.+)$/s);
  if (actMatch) return { mode: "act", instruction: actMatch[1].trim() };
  if (!c) return { mode: "archive" };
  // Name-like = short and no sentence punctuation → it's a label, not an
  // instruction. Anything else acts AND archives — asymmetric costs: a
  // false "act" wastes one session turn; a false "archive-only" forces a
  // re-ask (which still works — the session can find + Read the stored
  // path later).
  const words = c.split(/\s+/).length;
  const sentencey = /[?!.:]/.test(c);
  if (words <= 5 && !sentencey) return { mode: "archive", name: c };
  return { mode: "act", instruction: c };
}
