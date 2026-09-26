// Component tests for the intake item page. The dashboard has no DOM test
// environment (no jsdom), so these render to static HTML with
// react-dom/server and read the markup: what is on screen for a given
// URL, stage and plan history. Effects (fetching, polling) do not run.

import { describe, expect, test } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import type { ReactElement } from "react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import IntakePage from "@/pages/IntakePage";
import type { IntakeItem } from "@/lib/api";
import { fixtureDetail, fixtureItem, fixtureMessage } from "@/lib/intake.fixtures";
import { ItemScreen } from "./ItemView";
import ItemThread, { Composer } from "./ItemThread";
import PlanPanel from "./PlanPanel";
import { planVersions } from "@/lib/intake";

const noop = () => {};
const noAct = async () => true;

function render(el: ReactElement, url = "/intake"): string {
  return renderToStaticMarkup(<MemoryRouter initialEntries={[url]}>{el}</MemoryRouter>);
}

/** Text of every <button> in the markup, whitespace collapsed. */
function buttons(html: string): string[] {
  return [...html.matchAll(/<button[^>]*>([\s\S]*?)<\/button>/g)].map((m) =>
    m[1].replace(/<[^>]+>/g, "").replace(/&#x27;/g, "'").replace(/\s+/g, " ").trim(),
  );
}

describe("deep link", () => {
  const page = (url: string) =>
    render(
      <Routes>
        <Route path="/intake" element={<IntakePage />} />
      </Routes>,
      url,
    );

  test("/intake?item=<n> opens that item", () => {
    const html = page("/intake?item=7");
    expect(html).toContain('data-item-id="7"');
    expect(html).toContain("item #7");
    // The way back to the list (shown below the lg breakpoint).
    expect(html).toContain('aria-label="back to items"');
    // The list column beside the item (shown from the lg breakpoint).
    expect(html).toContain('aria-label="items"');
  });

  test("/intake without an item is the list", () => {
    const html = page("/intake");
    expect(html).not.toContain("data-item-id");
    expect(html).toContain("issue pipeline");
  });

  test("an invalid item parameter falls back to the list", () => {
    expect(page("/intake?item=abc")).not.toContain("data-item-id");
  });
});

describe("actions shown per stage", () => {
  const plans = [
    { version: 1, text: "# Plan\n\nfirst", at: "2026-09-24T10:10:00.000Z" },
    { version: 2, text: "# Plan\n\nsecond", at: "2026-09-24T10:20:00.000Z" },
  ];
  const screen = (it: IntakeItem, extra: Parameters<typeof fixtureDetail>[1] = {}) =>
    buttons(render(<ItemScreen detail={fixtureDetail(it, { plans, ...extra })} onChange={noop} />));

  test("refinement with a proposed plan: approve that version, cancel", () => {
    const b = screen(fixtureItem({ stage: "refinement", plan_version: 2, plan_draft: "# Plan\n\nsecond" }));
    expect(b).toContain("approve plan v2");
    expect(b).toContain("cancel");
    expect(b).not.toContain("retry");
    expect(b.some((t) => t.startsWith("release"))).toBe(false);
    expect(b.some((t) => /comment/i.test(t))).toBe(false);
  });

  test("refinement while the agent is answering: no approval", () => {
    const b = screen(fixtureItem({ stage: "refinement", plan_version: 2, current_task_id: "task-1" }));
    expect(b.some((t) => t.startsWith("approve"))).toBe(false);
    expect(b).toContain("cancel");
  });

  test("held: release bound to the hold shown, cancel", () => {
    const held = fixtureItem({ stage: "held", hold_stage: "queued", hold_hash: "a1b2c3d4e5" });
    const html = render(
      <ItemScreen
        detail={fixtureDetail(held, {
          hidden: [{ location: "body", kind: "html_comment", line: 1, column: 5, start: 4, end: 14, text: "<!-- x -->" }],
          hidden_sources: [{ location: "body", text: "see <!-- x --> here" }],
        })}
        onChange={noop}
      />,
    );
    const b = buttons(html);
    expect(b).toContain("release (hold a1b2c3)");
    expect(b).toContain("cancel");
    expect(b).not.toContain("retry");
    // The full raw source with the hidden range marked.
    expect(html).toContain("<mark");
    expect(html).toContain("&lt;!-- x --&gt;</mark>");
  });

  test("failed and blocked: retry and cancel", () => {
    for (const stage of ["failed", "blocked"] as const) {
      const b = screen(fixtureItem({ stage, failed_stage: "implementation", error: "tests failed" }));
      expect(b).toContain("retry");
      expect(b).toContain("cancel");
      expect(b.some((t) => t.startsWith("approve"))).toBe(false);
    }
  });

  test("implementation: cancel only", () => {
    const b = screen(fixtureItem({ stage: "implementation", plan_version: 2, approved_version: 2, approved_plan: "x" }));
    expect(b).toContain("cancel");
    expect(b).not.toContain("retry");
    expect(b.some((t) => t.startsWith("approve"))).toBe(false);
  });

  test("closed: no actions and the conversation is closed", () => {
    const html = render(<ItemScreen detail={fixtureDetail(fixtureItem({ stage: "closed", pr_url: "https://example.invalid/pull/3" }), { plans })} onChange={noop} />);
    const b = buttons(html);
    expect(b).not.toContain("cancel");
    expect(b).not.toContain("retry");
    expect(b).not.toContain("send");
    expect(html).toContain("the conversation is closed");
    expect(html).toContain('href="https://example.invalid/pull/3"');
  });
});

describe("conversation", () => {
  const messages = [
    fixtureMessage(1, { author: "nucleus", body: "plan v1 posted" }),
    fixtureMessage(3, { author: "operator", via: "dashboard", body: "use the second option" }),
    fixtureMessage(2, { author: "agent", body: "Two options:\n\n- **first**\n- second" }),
    fixtureMessage(4, { author: "operator", via: "whatsapp", body: "ok", pending_agent: 1 }),
  ];

  test("oldest first; operator stamps name their source; agent Markdown is rendered; notes are muted", () => {
    const html = render(<ItemThread item={fixtureItem()} messages={messages} visible onSent={noop} />);
    const order = [...html.matchAll(/data-author="(\w+)"/g)].map((m) => m[1]);
    expect(order).toEqual(["nucleus", "agent", "operator", "operator"]);
    expect(html).toContain("<strong");
    expect(html).toContain(">dashboard<");
    expect(html).toContain(">whatsapp<");
    expect(html).toContain("not read by the agent yet");
  });

  test("the composer posts outside refinement too and shows the pipeline's refusal", () => {
    const notice = "Item #4 is in implementation; replies are read during refinement only.";
    const html = render(<Composer item={fixtureItem({ stage: "implementation" })} onSent={noop} initialNotice={notice} />);
    expect(html).toContain('role="status"');
    expect(html).toContain("Item #4 is in implementation");
    expect(buttons(html)).toContain("send");
  });

  test("during refinement the composer has no stage hint", () => {
    const html = render(<Composer item={fixtureItem()} onSent={noop} />);
    expect(html).not.toContain("the agent reads replies during refinement");
    expect(html).toContain('aria-label="reply to the refinement agent"');
  });
});

describe("plan version selector", () => {
  const it = fixtureItem({ plan_version: 3, plan_draft: "c" });
  const plans = [
    { version: 1, text: "## Steps\n\n1. read\n2. edit", at: "2026-09-24T10:10:00.000Z" },
    { version: 2, text: "## Steps\n\n1. read\n2. edit\n3. test", at: "2026-09-24T10:20:00.000Z" },
    { version: 3, text: "## Steps\n\n1. read\n2. change\n3. test", at: "2026-09-24T10:30:00.000Z" },
  ];
  const versions = planVersions({ item: it, plans });
  const panel = (props: Partial<Parameters<typeof PlanPanel>[0]> = {}) =>
    render(<PlanPanel item={it} versions={versions} busy={false} act={noAct} {...props} />);

  test("lists every version, newest first, and shows the latest", () => {
    const html = panel();
    const options = [...html.matchAll(/<option[^>]*value="(\d+)"[^>]*>/g)].map((m) => m[1]);
    expect(options).toEqual(["3", "2", "1"]);
    expect(html).toMatch(/<option[^>]*value="3"[^>]*selected=""/);
    expect(html).toContain('data-plan-version="3"');
    expect(html).toContain("change");
    expect(html).toContain("proposed, not approved");
    expect(buttons(html)).toContain("approve plan v3");
    expect(buttons(html)).toContain("changes from v2");
  });

  test("an older version shows without the approve button", () => {
    const html = panel({ defaultVersion: 1 });
    expect(html).toContain('data-plan-version="1"');
    expect(html).toContain("replaced by a later version");
    expect(buttons(html).some((t) => t.startsWith("approve"))).toBe(false);
    expect(buttons(html)).toContain("no earlier version");
  });

  test("the changes view marks added and removed lines against the previous version", () => {
    const html = panel({ defaultView: "changes" });
    // Each row: its kind, then the line text (the +/− sign is a separate span).
    const rows = [...html.matchAll(/data-diff="(\w+)"[^>]*><span[^>]*>[^<]*<\/span>([\s\S]*?)<\/div>/g)].map((m) => `${m[1]}:${m[2]}`);
    expect(rows).toContain("del:2. edit");
    expect(rows).toContain("add:2. change");
    expect(rows).toContain("same:3. test");
    expect(rows.filter((r) => !r.startsWith("same:"))).toEqual(["del:2. edit", "add:2. change"]);
  });

  test("with no versions the panel says so", () => {
    expect(panel({ versions: [] })).toContain("no plan yet");
  });
});
