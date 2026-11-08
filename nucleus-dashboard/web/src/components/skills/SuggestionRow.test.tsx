// Component tests for the skill suggestions on the Skills page. Static
// render (react-dom/server, no DOM environment): effects and fetches do not
// run, so these read what the markup shows for a given review.

import { describe, expect, test } from "vitest";
import { renderToStaticMarkup } from "react-dom/server";
import type { ReactElement } from "react";
import { MemoryRouter, Route, Routes } from "react-router-dom";
import type { SuggestionReview, SuggestionSummary } from "@/lib/api";
import SkillsPage from "@/pages/SkillsPage";
import SuggestionRow, { SuggestionScreen, type Step, type View } from "./SuggestionRow";

const noop = () => {};

const DRAFT = "---\nname: service-deploy\ndescription: Deploy a service\nflavor: learned\n---\n\n# When to invoke\nWhen deploying.\n\n# Steps\n1. Build and test.\n\n# Failure modes\n- The push fails.\n";
const LIVE = DRAFT.replace("1. Build and test.", "1. Build.");

function review(over: Partial<SuggestionReview["suggestion"]> = {}, rest: Partial<Omit<SuggestionReview, "suggestion">> = {}): SuggestionReview {
  return {
    suggestion: {
      id: 2,
      kind: "create",
      name: "service-deploy",
      draft: DRAFT,
      rationale: "The operator corrected the deploy order in two sessions.",
      base_hash: null,
      origin: "backfill",
      status: "pending",
      reject_reason: null,
      superseded_by: null,
      overrides_rejected: null,
      why_new: null,
      edited: false,
      retires: [],
      created_at: "2026-11-03T15:00:00.000Z",
      updated_at: "2026-11-03T15:00:00.000Z",
      decided_at: null,
      evidence: [
        {
          id: 1,
          session_id: "sess-0001",
          source: "index",
          agent: null,
          note: "",
          quotes: ["USER: build before you push"],
          created_at: "2026-11-03T15:00:00.000Z",
        },
      ],
      ...over,
    },
    draft_hash: "ab".repeat(32),
    live: null,
    accept_blockers: [],
    ...rest,
  };
}

function screen(r: SuggestionReview, initialStep: Step = "none", initialView: View = "rendered"): string {
  return renderToStaticMarkup(
    <SuggestionScreen review={r} onOpen={noop} onChanged={noop} onLibraryChanged={noop} onDecided={noop} initialStep={initialStep} initialView={initialView} />,
  );
}

function render(el: ReactElement, url = "/skills"): string {
  return renderToStaticMarkup(<MemoryRouter initialEntries={[url]}>{el}</MemoryRouter>);
}

function buttons(html: string): string[] {
  return [...html.matchAll(/<button[^>]*>([\s\S]*?)<\/button>/g)].map((m) =>
    m[1].replace(/<[^>]+>/g, "").replace(/\s+/g, " ").trim(),
  );
}

describe("a pending suggestion", () => {
  test("shows the rationale, the rendered draft, the evidence and the three actions", () => {
    const html = screen(review());
    expect(html).toContain("The operator corrected the deploy order");
    expect(html).toContain("When to invoke</h");
    expect(html).toContain("description: Deploy a service");
    expect(html).toContain("sess-0001");
    expect(html).toContain("session index");
    expect(html).toContain("USER: build before you push");
    const b = buttons(html);
    expect(b).toEqual(expect.arrayContaining(["accept", "reject", "edit draft", "rendered", "raw"]));
    expect(b).not.toContain("changes from the live skill");
  });

  test("the raw view shows the SKILL.md text as stored", () => {
    expect(screen(review(), "none", "raw")).toContain("---\nname: service-deploy\n");
  });

  test("accept and reject each ask a second step; reject takes a reason", () => {
    const accept = screen(review(), "accept");
    expect(accept).toContain('role="alertdialog"');
    expect(accept).toContain("Create the private skill service-deploy at .nucleus/.claude/skills/service-deploy/SKILL.md?");
    expect(buttons(accept)).toEqual(expect.arrayContaining(["accept", "keep"]));
    const reject = screen(review(), "reject");
    expect(reject).toContain("Reject #2? Reason (optional");
    expect(reject).toContain("<input");
    expect(buttons(reject)).toEqual(expect.arrayContaining(["reject", "keep"]));
  });

  test("edit shows the draft in a text area with save and cancel", () => {
    const html = screen(review(), "edit");
    expect(html).toContain("<textarea");
    expect(html).toContain("# Failure modes");
    expect(buttons(html)).toEqual(expect.arrayContaining(["save draft", "cancel"]));
  });

  test("the server's accept blockers are listed as it returned them", () => {
    const html = screen(review({}, { accept_blockers: ["a skill named `service-deploy` already exists (tier repo)"] }));
    expect(html).toContain("Accept is refused now:");
    expect(html).toContain("a skill named `service-deploy` already exists (tier repo)");
  });
});

describe("a patch suggestion", () => {
  const patch = (state: "unchanged" | "changed") =>
    review(
      { kind: "patch", base_hash: "ab".repeat(32) },
      {
        live: { path: "/ws/.nucleus/.claude/skills/service-deploy/SKILL.md", text: LIVE, state, problem: null },
        accept_blockers: state === "changed" ? ["the skill `service-deploy` changed after the draft was written"] : [],
      },
    );

  test("offers the changes against the live skill", () => {
    const html = screen(patch("unchanged"), "none", "changes");
    expect(buttons(html)).toContain("changes from the live skill");
    expect(html).toContain('data-diff="del"');
    expect(html).toContain('data-diff="add"');
    expect(html).toContain("1. Build and test.");
    expect(html).toContain("has not changed since this suggestion was written");
  });

  test("says when the live skill changed and shows core's refusal", () => {
    const html = screen(patch("changed"));
    expect(html).toContain("The live skill changed after this suggestion was written");
    expect(html).toContain("the skill `service-deploy` changed after the draft was written");
  });
});

test("a curator consolidation names the skills accept retires, in the view and the confirmation", () => {
  const r = review({ kind: "patch", origin: "curator", retires: ["deploy-web", "deploy-api"] });
  const html = screen(r);
  expect(html).toContain("retires · 2 skills");
  expect(html).toContain("<code");
  expect(html).toContain("deploy-web");
  expect(html).toContain("curator");
  expect(screen(r, "accept")).toContain("Accept also moves the 2 merged skills deploy-web, deploy-api to .nucleus/.claude/skills/.archive/");
  expect(screen(review())).not.toContain("retires ·");
});

test("a decided suggestion has no actions", () => {
  const html = screen(review({ status: "rejected", reject_reason: "too narrow", decided_at: "2026-11-03T16:00:00.000Z" }));
  const b = buttons(html);
  for (const a of ["accept", "reject", "edit draft"]) expect(b).not.toContain(a);
  expect(html).toContain("reason: too narrow");
});

test("a collapsed row shows kind, origin, evidence count and the rationale on one line", () => {
  const s: SuggestionSummary = {
    id: 2,
    kind: "patch",
    name: "service-deploy",
    status: "pending",
    origin: "live",
    rationale: "line one\nline two",
    reject_reason: null,
    superseded_by: null,
    edited: true,
    evidence_count: 3,
    created_at: "2026-11-03T15:00:00.000Z",
    updated_at: "2026-11-03T15:00:00.000Z",
  };
  const html = render(<SuggestionRow summary={s} open={false} onToggle={noop} onOpen={noop} onChanged={noop} onLibraryChanged={noop} onDecided={noop} />);
  expect(html).toContain("#2");
  expect(html).toContain("service-deploy");
  expect(html).toContain("patch");
  expect(html).toContain("live sweep");
  expect(html).toContain("3 sessions of evidence");
  expect(html).toContain("line one line two");
  expect(html).toContain("edited");
  expect(html).toContain('aria-expanded="false"');
});

test("the Skills page has the suggestions section above the library", () => {
  const html = render(
    <Routes>
      <Route path="/skills" element={<SkillsPage />} />
    </Routes>,
    "/skills?suggestion=2",
  );
  const sug = html.indexOf('aria-label="skill suggestions"');
  const lib = html.indexOf("library");
  expect(sug).toBeGreaterThan(-1);
  expect(lib).toBeGreaterThan(sug);
});
