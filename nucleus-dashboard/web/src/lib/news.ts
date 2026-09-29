// News page logic that does not need React: the downvote reasons the widget
// offers, and how the ranked list splits into the page's sections.

import type { NewsItem } from "./api";

/** The reasons a downvote may carry, in the widget's order and wording
 *  (ADR-031). The keys are the fetcher's `VOTE_REASONS`; `other` needs a note. */
export const VOTE_REASONS = [
  { key: "dup", label: "dup" },
  { key: "old", label: "old" },
  { key: "knew-it", label: "knew it" },
  { key: "off-topic", label: "off-topic" },
  { key: "weak-piece", label: "weak piece" },
  { key: "other", label: "other…" },
] as const;

export type VoteReason = (typeof VOTE_REASONS)[number]["key"];

/** Same cap as the fetcher's `MAX_VOTE_NOTE_CHARS` and the widget's editor. */
export const MAX_VOTE_NOTE_CHARS = 500;

/** How a stored reason reads on the card. A key this build does not know is
 *  shown as stored. */
export function reasonChipLabel(key: string): string {
  const known = VOTE_REASONS.find((r) => r.key === key);
  return known ? known.label.replace("…", "") : key;
}

export const NOTABLE_THRESHOLD = 0.6;

export type NewsSections = {
  hero: NewsItem | null;
  notable: NewsItem[];
  rest: NewsItem[];
};

/** Split the ranked list into the hero, the notable grid and the rest.
 *
 *  The API returns items of one event next to each other, under the
 *  best-scored one. A section is chosen per event from that first item, so a
 *  lower-scored write-up stays next to the story it belongs to instead of
 *  moving to another section. The hero's own event continues in `notable`. */
export function splitItems(items: NewsItem[]): NewsSections {
  if (items.length === 0) return { hero: null, notable: [], rest: [] };
  const [hero, ...others] = items;
  const notable: NewsItem[] = [];
  const rest: NewsItem[] = [];
  const sectionOfEvent = new Map<string, NewsItem[]>();
  if (hero.event_slug) sectionOfEvent.set(hero.event_slug, notable);
  for (const it of others) {
    const slug = it.event_slug;
    let section = slug ? sectionOfEvent.get(slug) : undefined;
    if (!section) {
      section = (it.notable_score ?? 0) >= NOTABLE_THRESHOLD ? notable : rest;
      if (slug) sectionOfEvent.set(slug, section);
    }
    section.push(it);
  }
  return { hero, notable, rest };
}

/** The visible items in the API's order: best score first (unscored last),
 *  then newest, with each event's items following its best-scored one.
 *
 *  The API already returns that order, but a source filter can hide an
 *  event's first item and leave a lower-scored write-up of it ahead of better
 *  items, so the page orders what it shows again. */
export function orderForDisplay(items: NewsItem[]): NewsItem[] {
  const score = (it: NewsItem) => it.notable_score ?? Number.NEGATIVE_INFINITY;
  const sorted = [...items].sort(
    (a, b) => score(b) - score(a) || b.published_at.localeCompare(a.published_at),
  );
  const out: NewsItem[] = [];
  const placed = new Set<string>();
  sorted.forEach((it, i) => {
    if (placed.has(it.id)) return;
    placed.add(it.id);
    out.push(it);
    if (!it.event_slug) return;
    for (const other of sorted.slice(i + 1)) {
      if (!placed.has(other.id) && other.event_slug === it.event_slug) {
        placed.add(other.id);
        out.push(other);
      }
    }
  });
  return out;
}

/** Characters as the API counts them (Unicode scalar values), not UTF-16
 *  code units as `maxLength` would. */
export function clampNote(text: string): string {
  const chars = [...text];
  return chars.length > MAX_VOTE_NOTE_CHARS ? chars.slice(0, MAX_VOTE_NOTE_CHARS).join("") : text;
}

/** Event slugs that more than one listed item shares. Only those are worth
 *  labelling: a slug on its own groups nothing. */
export function sharedEvents(items: NewsItem[]): Set<string> {
  const counts = new Map<string, number>();
  for (const it of items) {
    if (it.event_slug) counts.set(it.event_slug, (counts.get(it.event_slug) ?? 0) + 1);
  }
  return new Set([...counts].filter(([, n]) => n > 1).map(([slug]) => slug));
}

/** Runs tasks one at a time, in the order they were given. A failed task does
 *  not stop the ones after it. */
export function serialQueue() {
  let tail: Promise<unknown> = Promise.resolve();
  return <T>(task: () => Promise<T>): Promise<T> => {
    const run = tail.then(task);
    tail = run.catch(() => undefined);
    return run;
  };
}
