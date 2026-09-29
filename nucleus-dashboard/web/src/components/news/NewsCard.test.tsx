// Static-markup tests (the dashboard has no DOM test environment; see
// components/work/ItemPage.test.tsx). Effects and clicks do not run.

import { describe, expect, it } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import type { NewsItem } from "@/lib/api";
import NewsCard, { type NewsCardActions } from "./NewsCard";
import NewsBriefTile from "./NewsBriefTile";

const actions: NewsCardActions = {
  onVote: () => {},
  onReason: async () => true,
  onReasonOpen: () => {},
  onReasonClose: () => {},
  onOpen: () => {},
};

function item(over: Partial<NewsItem>): NewsItem {
  return {
    id: "i1",
    source_id: 1,
    source_name: "Feed",
    url: "https://discuss.example/i1",
    article_url: "https://article.example/i1",
    title: "A headline",
    summary: null,
    published_at: "2026-09-13T09:00:00.000Z",
    published_date: "2026-09-13",
    fetch_date: "2026-09-13",
    notable_score: 0.5,
    notable_reason: null,
    event_slug: "some-event",
    stale: 0,
    vote: 0,
    vote_reason: null,
    vote_note: null,
    opened: false,
    ...over,
  };
}

const card = (it: NewsItem, opts: { reasonOpen?: boolean; sharedEvent?: boolean } = {}) =>
  renderToStaticMarkup(
    <NewsCard
      item={it}
      variant="rest"
      reasonOpen={opts.reasonOpen ?? false}
      sharedEvent={opts.sharedEvent ?? false}
      actions={actions}
    />,
  );

const buttons = (html: string) =>
  [...html.matchAll(/<button[^>]*>([\s\S]*?)<\/button>/g)].map((m) => m[1].replace(/<[^>]+>/g, "").trim());

describe("NewsCard", () => {
  it("offers why? on a downvote without a reason", () => {
    expect(buttons(card(item({ vote: -1 })))).toContain("why?");
    expect(buttons(card(item({ vote: 0 })))).not.toContain("why?");
  });

  it("reads a stored reason back as a chip with its note as the tooltip", () => {
    const html = card(item({ vote: -1, vote_reason: "other", vote_note: "covered last week" }));
    expect(buttons(html)).toContain("other");
    expect(html).toContain('title="covered last week"');
  });

  it("replaces the meta line with the widget's reasons while the strip is open", () => {
    const html = card(item({ vote: -1 }), { reasonOpen: true });
    const labels = buttons(html);
    expect(labels).toEqual(expect.arrayContaining(["dup", "old", "knew it", "off-topic", "weak piece", "other…"]));
    expect(labels).not.toContain("why?");
    expect(html).not.toContain("pub 2026-09-13");
  });

  it("marks an opened item and labels an event only when another item shares it", () => {
    expect(card(item({ opened: true }))).toContain('aria-label="opened"');
    expect(card(item({ opened: false }))).not.toContain('aria-label="opened"');
    expect(card(item({}), { sharedEvent: true })).toContain("event some-event");
    expect(card(item({}), { sharedEvent: false })).not.toContain("event some-event");
  });
});

describe("NewsBriefTile", () => {
  const brief = (standing: "current" | "names_downvoted" | "unverifiable") =>
    renderToStaticMarkup(
      <NewsBriefTile brief={{ run_id: "r1", created_at: "2026-09-13T22:00:00.000Z", text: "Read this.", standing }} />,
    );

  it("says why the widget would no longer show a brief", () => {
    expect(brief("current")).not.toContain("would not show");
    expect(brief("names_downvoted")).toContain("an item you downvoted since");
    expect(brief("unverifiable")).toContain("cannot be checked");
  });
});
