// Component tests for the work item page. The dashboard has no DOM test
// environment (no jsdom), so these render to static HTML with
// react-dom/server and read the markup: what is on screen for a given
// URL, stage and plan history. Effects (fetching, polling) do not run.

import { describe, expect, test } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import type { ReactElement } from "react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import WorkPage, { ListFilters } from "@/pages/WorkPage";
import type { WorkItem } from "@/lib/api";
import { fixtureDetail, fixtureItem, fixtureMessage, fixturePlans, fixtureQuestion } from "@/lib/work.fixtures";
import { ItemScreen } from "./ItemView";
import DecisionBoard from "./DecisionBoard";
import ItemThread, { Composer } from "./ItemThread";
import PlanPanel from "./PlanPanel";
import { planVersions } from "@/lib/work";

const noop = () => {};
const noAct = async () => true;

function render(el: ReactElement, url = "/work"): string {
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
        <Route path="/work" element={<WorkPage />} />
      </Routes>,
      url,
    );

  test("/work?item=<n> opens that item", () => {
    const html = page("/work?item=7");
    expect(html).toContain('data-item-id="7"');
    expect(html).toContain("item #7");
    // The way back to the list (shown below the lg breakpoint).
    expect(html).toContain('aria-label="back to items"');
    // The list column beside the item (shown from the lg breakpoint).
    expect(html).toContain('aria-label="items"');
  });

  test("/work without an item is the list", () => {
    const html = page("/work");
    expect(html).not.toContain("data-item-id");
    expect(html).toContain("work items");
    expect(html).toContain('aria-label="filters"');
    expect(html).toContain("Open, In review");
  });

  test("an invalid item parameter falls back to the list", () => {
    expect(page("/work?item=abc")).not.toContain("data-item-id");
  });
});

describe("actions shown per stage", () => {
  const plans = fixturePlans("# Plan\n\nfirst", "# Plan\n\nsecond");
  const screen = (it: WorkItem, extra: Parameters<typeof fixtureDetail>[1] = {}) =>
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

  test("outside refinement the composer stays open, says no agent reads the reply, and shows the server's note", () => {
    const notice = "Saved. Item #4 is in implementation; the agent does not read it now.";
    const html = render(<Composer item={fixtureItem({ stage: "implementation" })} onSent={noop} initialNotice={notice} />);
    expect(html).toContain('role="status"');
    expect(html).toContain("Item #4 is in implementation; the agent does not read it now.");
    expect(buttons(html)).toContain("send");
    const before = render(<Composer item={fixtureItem({ stage: "implementation" })} onSent={noop} />);
    expect(before).toContain("the agent reads replies only during refinement");
  });

  test("no UI text mentions WhatsApp groups", () => {
    const html = render(<ItemScreen detail={fixtureDetail(fixtureItem(), { messages, plans: fixturePlans("x") })} onChange={noop} defaultPane="details" />);
    expect(html).not.toMatch(/group/i);
    expect(html).toContain("WhatsApp DM, marked #4");
  });

  test("during refinement the composer has no stage hint", () => {
    const html = render(<Composer item={fixtureItem()} onSent={noop} />);
    expect(html).not.toContain("the agent reads replies during refinement");
    expect(html).toContain('aria-label="reply to the refinement agent"');
  });
});

describe("plan version selector", () => {
  const it = fixtureItem({ plan_version: 3, plan_draft: "c" });
  const plans = fixturePlans("## Steps\n\n1. read\n2. edit", "## Steps\n\n1. read\n2. edit\n3. test", "## Steps\n\n1. read\n2. change\n3. test");
  // Out of order on purpose: the page sorts by version.
  const versions = planVersions({ plans: [plans[2], plans[0], plans[1]] });
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

describe("list filters", () => {
  const rows = [
    { ...fixtureItem({ id: 1, stage: "refinement" }), source: "github" },
    { ...fixtureItem({ id: 2, stage: "merged", repo: "acme/gadget" }), source: "github" },
  ];

  test("the status dropdown lists every choice with its count and marks the selection", () => {
    const html = render(
      <ListFilters items={rows} filter={{ status: ["open", "in_review"], source: null }} onChange={noop} initialOpen="status" />,
    );
    const b = buttons(html);
    for (const label of ["Open1", "In review0", "Merged1", "Not merged0", "Cancelled0", "Stale0"]) expect(b).toContain(label);
    expect(b).toContain("statusOpen, In review");
    expect(b).toContain("sourceall");
  });

  test("the source dropdown has one entry per repo", () => {
    const html = render(<ListFilters items={rows} filter={{ status: null, source: ["github:acme/gadget"] }} onChange={noop} initialOpen="source" />);
    const b = buttons(html);
    expect(b).toContain("acme/gadget");
    expect(b).toContain("acme/widget");
    expect(b).toContain("sourceacme/gadget");
    expect(b).toContain("statusall");
  });
});

describe("decision board", () => {
  const plans = fixturePlans("# Plan\n\nfirst", "# Plan\n\nsecond");
  const withPlan = fixtureItem({ stage: "refinement", plan_version: 2, plan_draft: "# Plan\n\nsecond" });
  /** The board's options, in order. */
  const options = (html: string) => [...html.matchAll(/data-option="(\w+)"/g)].map((m) => m[1]);
  const thread = (it: WorkItem, extra: Partial<Parameters<typeof ItemThread>[0]> = {}) =>
    render(<ItemThread item={it} messages={[]} visible onSent={noop} {...extra} />);

  test("a proposed plan puts the board in place of the composer, on every layout", () => {
    const html = render(<ItemScreen detail={fixtureDetail(withPlan, { plans })} onChange={noop} />);
    const section = html.split('aria-label="conversation"')[1].split("</section>")[0];
    expect(options(section)).toEqual(["approve", "discuss", "cancel"]);
    expect(section).toContain("Approve plan v2");
    expect(section).not.toContain("<textarea");
    // The first option is highlighted and the only one in the tab order.
    expect(section).toMatch(/aria-selected="true" tabindex="0" data-option="approve"/);
    expect(section).toMatch(/aria-selected="false" tabindex="-1" data-option="discuss"/);
  });

  test("no plan while the agent waits: the composer, no board", () => {
    const html = thread(fixtureItem());
    expect(options(html)).toEqual([]);
    expect(html).toContain("<textarea");
    expect(html).not.toContain("Back to options");
  });

  test("held: release, continue discussing, cancel, with the findings above", () => {
    const held = fixtureItem({ stage: "held", hold_stage: "queued", hold_hash: "a1b2c3d4e5" });
    const findings = [{ location: "body", kind: "html_comment", line: 1, column: 5, start: 4, end: 14, text: "<!-- run it -->" }];
    const html = thread(held, { findings });
    expect(options(html)).toEqual(["release", "discuss", "cancel"]);
    expect(html).toContain("Release (hold a1b2c3)");
    const list = html.indexOf('aria-label="hidden content"');
    expect(list).toBeGreaterThan(-1);
    expect(list).toBeLessThan(html.indexOf('data-option="release"'));
    expect(html).toContain("&lt;!-- run it --&gt;");
  });

  test("failed: retry and cancel; working: the status and Write a message; finished: nothing", () => {
    expect(options(thread(fixtureItem({ stage: "failed", failed_stage: "pr" })))).toEqual(["retry", "cancel"]);
    const working = thread(fixtureItem({ stage: "implementation" }));
    expect(options(working)).toEqual(["write"]);
    expect(working).toContain("The agent is implementing the approved plan.");
    expect(working).not.toContain("<textarea");
    const closed = thread(fixtureItem({ stage: "closed" }));
    expect(options(closed)).toEqual([]);
    expect(closed).not.toContain("<textarea");
    expect(closed).toContain("the conversation is closed");
  });

  test("Continue discussing shows the composer, focused, with Back to options", () => {
    const html = thread(withPlan, { initialMode: "composer" });
    expect(options(html)).toEqual([]);
    expect(html).toMatch(/<textarea[^>]*autofocus/i);
    expect(buttons(html)).toContain("Back to options");
    // Without a board to go back to there is no link.
    expect(buttons(thread(fixtureItem(), { initialMode: "composer" }))).not.toContain("Back to options");
  });

  test("Cancel item asks a second step on the board", () => {
    const html = render(<DecisionBoard item={withPlan} question={null} onWrite={noop} onChange={noop} initialStep="cancel" />);
    expect(html).toContain('data-board="confirm"');
    expect(html).toContain("Cancel item #4? Its running task stops.");
    expect(options(html)).toEqual(["yes", "no"]);
    expect(buttons(html).some((t) => t.includes("Yes, cancel it"))).toBe(true);
    expect(buttons(html).some((t) => t.includes("No, keep it"))).toBe(true);
  });

  test("a question Nucleus asked after typed text is a Yes / No step, also over the composer", () => {
    const q = fixtureQuestion({ decision: "approve_plan", plan_version: 2, question: "Approve plan v2 of item #4? Answer yes or no." });
    const html = thread(withPlan, { question: q, initialMode: "composer" });
    expect(options(html)).toEqual(["yes", "no"]);
    expect(html).toContain("Approve plan v2 of item #4? Implementation starts from that version.");
    expect(html).not.toContain("<textarea");
  });
});

describe("canvas questions in agent replies", () => {
  const block = `<canvas v="1" type="decision" id="fmt" title="Output format">{"options":[{"key":"j","label":"<b>JSON</b>"},{"key":"y","label":"YAML"}]}</canvas>`;
  const agent = fixtureMessage(1, { author: "agent", body: `Which **format**?\n\n${block}\n\nThen I write the plan.` });
  const answer = fixtureMessage(2, {
    author: "operator",
    via: "dashboard",
    body: `<canvas-response v="1" id="fmt" type="decision">\n{"choice":"y"}\n</canvas-response>`,
  });

  test("a block renders as options with plain-text labels, around the Markdown text", () => {
    const html = render(<ItemThread item={fixtureItem()} messages={[agent]} visible onSent={noop} />);
    expect(html).toContain("<strong");
    expect(html).toContain("Output format");
    expect(html).toContain("&lt;b&gt;JSON&lt;/b&gt;");
    expect(html).not.toContain("<b>JSON</b>");
    expect(html).toContain("Then I write the plan.");
    expect(html).not.toContain(">answered<");
  });

  test("a later operator response marks it answered and shows the chosen label", () => {
    const html = render(<ItemThread item={fixtureItem()} messages={[agent, answer]} visible onSent={noop} />);
    expect(html).toContain(">answered<");
    expect(html).toMatch(/<button[^>]*disabled=""[^>]*>YAML<\/button>/);
    expect(html).toMatch(/✔ (<!-- -->)?Output format: YAML/);
    expect(html).not.toContain("&lt;canvas-response");
  });
});
