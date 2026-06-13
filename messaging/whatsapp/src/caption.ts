// ADR-018/013 inbound-media caption heuristics — separate module so tests
// can import it without executing index.ts's top-level main().

export interface CaptionDecision {
  mode: "archive" | "act" | "vault-import";
  name?: string;
  instruction?: string;
  /** ADR-013 `priv:` — archive-only AND never enriched (no session ever
   *  reads the bytes; the full by-reference posture). */
  noEnrich?: boolean;
}

/** Act-vs-archive-vs-import decision for an inbound media caption. */
export function classifyCaption(caption: string): CaptionDecision {
  const c = caption.trim();
  // Escape hatches first.
  const privMatch = c.match(/^priv:\s*(.*)$/is);
  if (privMatch) {
    const name = privMatch[1].trim();
    return { mode: "archive", ...(name ? { name } : {}), noEnrich: true };
  }
  const importMatch = c.match(/^(?:vault|import):\s*(.*)$/is);
  if (importMatch) {
    const rest = importMatch[1].trim();
    // A short remainder is the logical name; anything longer is ignored
    // for naming (filename heuristic applies) but still imports.
    const nameLike = rest && rest.split(/\s+/).length <= 5 && !/[?!.:]/.test(rest);
    return { mode: "vault-import", ...(nameLike ? { name: rest } : {}) };
  }
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
