import { describe, expect, it } from "vitest";
import type { NewsItem } from "./api";
import {
  MAX_VOTE_NOTE_CHARS,
  clampNote,
  orderForDisplay,
  reasonChipLabel,
  serialQueue,
  sharedEvents,
  splitItems,
} from "./news";

function item(id: string, score: number | null, event: string | null = null, source = "Feed"): NewsItem {
  return {
    id,
    source_id: 1,
    source_name: source,
    url: `https://example.com/${id}`,
    article_url: null,
    title: id,
    summary: null,
    published_at: "2026-09-13T09:00:00.000Z",
    published_date: "2026-09-13",
    fetch_date: "2026-09-13",
    notable_score: score,
    notable_reason: null,
    event_slug: event,
    stale: 0,
    vote: 0,
    vote_reason: null,
    vote_note: null,
    opened: false,
  };
}

const ids = (items: NewsItem[]) => items.map((i) => i.id);

describe("splitItems", () => {
  it("splits by score when no events are shared", () => {
    const s = splitItems([item("a", 0.9), item("b", 0.7), item("c", 0.3)]);
    expect(s.hero?.id).toBe("a");
    expect(ids(s.notable)).toEqual(["b"]);
    expect(ids(s.rest)).toEqual(["c"]);
  });

  it("keeps a low-scored write-up in the section of its event", () => {
    const s = splitItems([
      item("a", 0.9),
      item("b", 0.7, "rubygems"),
      item("c", 0.4, "rubygems"),
      item("d", 0.3),
    ]);
    expect(ids(s.notable)).toEqual(["b", "c"]);
    expect(ids(s.rest)).toEqual(["d"]);
  });

  it("continues the hero's event at the top of the notable grid", () => {
    const s = splitItems([item("a", 0.9, "release"), item("b", 0.2, "release"), item("c", 0.7)]);
    expect(ids(s.notable)).toEqual(["b", "c"]);
    expect(s.rest).toEqual([]);
  });

  it("returns nothing for an empty list", () => {
    expect(splitItems([])).toEqual({ hero: null, notable: [], rest: [] });
  });
});

describe("sharedEvents", () => {
  it("lists only slugs that more than one item carries", () => {
    const shared = sharedEvents([item("a", 1, "x"), item("b", 1, "x"), item("c", 1, "y"), item("d", 1)]);
    expect([...shared]).toEqual(["x"]);
  });
});

describe("reasonChipLabel", () => {
  it("uses the widget's wording without the ellipsis", () => {
    expect(reasonChipLabel("knew-it")).toBe("knew it");
    expect(reasonChipLabel("other")).toBe("other");
    expect(reasonChipLabel("future-key")).toBe("future-key");
  });
});

describe("orderForDisplay", () => {
  it("puts the best visible item first when a filter hides an event's leader", () => {
    const api = [item("a", 0.9, "e", "X"), item("c", 0.2, "e", "Y"), item("b", 0.8, "f", "Y")];
    const visible = api.filter((i) => i.source_name === "Y");
    expect(ids(orderForDisplay(visible))).toEqual(["b", "c"]);
    expect(splitItems(orderForDisplay(visible)).hero?.id).toBe("b");
  });

  it("keeps an event's items together under its best-scored one and unscored items last", () => {
    const list = [item("n", null), item("c", 0.3, "e"), item("b", 0.5), item("a", 0.9, "e")];
    expect(ids(orderForDisplay(list))).toEqual(["a", "c", "b", "n"]);
  });
});

describe("clampNote", () => {
  it("counts characters, not UTF-16 code units", () => {
    const emoji = "😀".repeat(300);
    expect(clampNote(emoji)).toBe(emoji);
    expect([...clampNote("😀".repeat(MAX_VOTE_NOTE_CHARS + 5))].length).toBe(MAX_VOTE_NOTE_CHARS);
  });
});

describe("serialQueue", () => {
  it("starts a task only after the one before it settled, even when it failed", async () => {
    const enqueue = serialQueue();
    const log: string[] = [];
    let releaseFirst!: () => void;
    const first = enqueue(
      () =>
        new Promise<void>((resolve) => {
          log.push("downvote sent");
          releaseFirst = resolve;
        }),
    );
    const failing = enqueue(async () => {
      log.push("reason sent");
      throw new Error("offline");
    });
    const third = enqueue(async () => {
      log.push("upvote sent");
    });
    await Promise.resolve();
    expect(log).toEqual(["downvote sent"]);
    releaseFirst();
    await first;
    await expect(failing).rejects.toThrow("offline");
    await third;
    expect(log).toEqual(["downvote sent", "reason sent", "upvote sent"]);
  });
});
