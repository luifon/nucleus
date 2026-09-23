import { describe, expect, test } from "vitest";
import type { Skill, SkillActionResp } from "@/lib/api/skills";
import {
  collisionWarning,
  deleteConfirmText,
  describeResult,
  reminderLine,
  shortPath,
  skillActions,
  tierLabel,
} from "./skills";

function skill(over: Partial<Skill> = {}): Skill {
  return {
    name: "demo",
    dir_name: "demo",
    description: "",
    tier: "personal",
    path: "/work/ws/.nucleus/.claude/skills/demo/SKILL.md",
    symlink_target: null,
    shadowed_by: null,
    also_in: [],
    restore_name: null,
    flavor: null,
    created_by: null,
    pinned: false,
    mcp_needed: null,
    last_used: null,
    last_failure: null,
    failure_count_30d: null,
    notify_on_failure: null,
    tags: null,
    trigger: null,
    ...over,
  };
}

function resp(over: Partial<SkillActionResp> = {}): SkillActionResp {
  return {
    action: "move",
    from: "personal",
    dir_name: "demo",
    to: "global",
    new_dir_name: "demo",
    path: "/x/demo/SKILL.md",
    git: { status: "not-applicable" },
    note: null,
    ...over,
  };
}

const enabled = (s: Skill) =>
  skillActions(s).map((a) => [a.kind, a.label, a.disabledReason === null] as const);

describe("skillActions", () => {
  test("private rows move to global and archive", () => {
    expect(enabled(skill())).toEqual([
      ["move", "move to global", true],
      ["archive", "archive", true],
    ]);
  });

  test("global rows move to private and archive", () => {
    expect(enabled(skill({ tier: "global" }))).toEqual([
      ["move", "move to private", true],
      ["archive", "archive", true],
    ]);
  });

  test("repo rows and active symlinks have no actions", () => {
    expect(skillActions(skill({ tier: "repo" }))).toEqual([]);
    expect(skillActions(skill({ tier: "global", symlink_target: "../vendor/demo" }))).toEqual([]);
  });

  test("pinned disables archive only", () => {
    const [move, archive] = skillActions(skill({ pinned: true }));
    expect(move.disabledReason).toBeNull();
    expect(archive.disabledReason).toMatch(/pinned/);
  });

  test("a same-named copy in another active tier disables move", () => {
    const [move, archive] = skillActions(skill({ tier: "global", also_in: ["repo"] }));
    expect(move.disabledReason).toMatch(/also in repo/);
    expect(archive.disabledReason).toBeNull();
  });

  test("archived rows restore and delete; delete is marked dangerous", () => {
    const acts = skillActions(skill({ tier: "personal-archive" }));
    expect(acts.map((a) => [a.kind, a.disabledReason === null, a.danger])).toEqual([
      ["restore", true, false],
      ["delete", true, true],
    ]);
  });

  test("restore is disabled when the name is active elsewhere or for a symlink", () => {
    const blocked = skillActions(skill({ tier: "global-archive", also_in: ["global", "personal"] }));
    expect(blocked[0].disabledReason).toMatch(/active in global, private/);
    expect(blocked[1].disabledReason).toBeNull();

    const link = skillActions(skill({ tier: "global-archive", symlink_target: "../v" }));
    expect(link[0].disabledReason).toMatch(/symlink/);
    expect(link[1].disabledReason).toBeNull();
  });
});

describe("collisionWarning", () => {
  test("shadowed copy names the winning tier", () => {
    expect(collisionWarning(skill({ shadowed_by: "global", also_in: ["global"] }))).toBe(
      "not loaded: the global copy wins",
    );
  });

  test("the loading copy lists the other tiers", () => {
    expect(collisionWarning(skill({ tier: "global", also_in: ["repo", "personal"] }))).toBe(
      "also in repo, private; this copy loads",
    );
  });

  test("archived copy with an active namesake reports the blocked restore", () => {
    expect(collisionWarning(skill({ tier: "personal-archive", also_in: ["personal"] }))).toMatch(
      /^restore blocked/,
    );
  });

  test("no collision → null", () => {
    expect(collisionWarning(skill())).toBeNull();
    expect(collisionWarning(skill({ tier: "global-archive" }))).toBeNull();
  });
});

describe("shortPath", () => {
  test("each tier shortens to its documented location", () => {
    expect(shortPath("/w/ws/.nucleus/.claude/skills/a/SKILL.md", "personal")).toBe(
      ".nucleus/.claude/skills/a/SKILL.md",
    );
    expect(shortPath("/w/ws/.nucleus/.claude/skills/.archive/a-2026-01-02/SKILL.md", "personal-archive")).toBe(
      ".nucleus/.claude/skills/.archive/a-2026-01-02/SKILL.md",
    );
    expect(shortPath("/w/ws/.claude/skills/a/SKILL.md", "repo")).toBe(".claude/skills/a/SKILL.md");
    expect(shortPath("/h/u/.claude/skills/a/SKILL.md", "global")).toBe("~/.claude/skills/a/SKILL.md");
    expect(shortPath("/h/u/.claude/skills-archive/a/SKILL.md", "global-archive")).toBe(
      "~/.claude/skills-archive/a/SKILL.md",
    );
  });

  test("unknown layout is returned unchanged", () => {
    expect(shortPath("/elsewhere/a/SKILL.md", "global")).toBe("/elsewhere/a/SKILL.md");
  });
});

describe("deleteConfirmText", () => {
  test("private archive is recoverable, global archive is permanent", () => {
    expect(deleteConfirmText(skill({ tier: "personal-archive" }))).toMatch(/Recoverable from the \.nucleus git history/);
    expect(deleteConfirmText(skill({ tier: "global-archive" }))).toMatch(/permanent/);
  });

  test("a symlink mentions that only the link is removed", () => {
    expect(deleteConfirmText(skill({ tier: "global-archive", symlink_target: "../v" }))).toMatch(
      /Only the symlink is removed/,
    );
  });
});

describe("describeResult", () => {
  test("move without git is ok", () => {
    expect(describeResult(resp())).toEqual({ kind: "ok", lines: ["moved demo from private to global"] });
  });

  test("archive with a renamed directory and a commit", () => {
    const r = describeResult(
      resp({
        action: "archive",
        to: "personal-archive",
        new_dir_name: "demo-2026-09-23",
        git: { status: "committed", sha: "0123456789abcdef" },
      }),
    );
    expect(r.kind).toBe("ok");
    expect(r.lines).toEqual([
      "archived demo from private to private archive as demo-2026-09-23",
      "git: committed 0123456 in .nucleus",
    ]);
  });

  test("failed commit is a warning and keeps the note", () => {
    const r = describeResult(
      resp({
        action: "delete",
        from: "personal-archive",
        to: null,
        new_dir_name: null,
        git: { status: "failed", error: "index.lock exists" },
        note: "deleted; recoverable …",
      }),
    );
    expect(r.kind).toBe("warn");
    expect(r.lines[0]).toBe("deleted demo from private archive");
    expect(r.lines[1]).toMatch(/^git commit failed: index\.lock exists/);
    expect(r.lines[2]).toBe("deleted; recoverable …");
  });

  test("skipped commit shows the reason", () => {
    const r = describeResult(resp({ git: { status: "skipped", reason: "no .git" } }));
    expect(r.lines[1]).toBe("git: no commit (no .git)");
  });
});

test("tierLabel and reminderLine", () => {
  expect(tierLabel("personal")).toBe("private");
  expect(tierLabel("global-archive")).toBe("global archive");
  expect(reminderLine({ id: 12, title: "digest", field: "system_prompt" })).toBe("#12 digest (system_prompt)");
  expect(reminderLine({ id: 3, title: null, field: "fallback_cmd" })).toBe("#3 (no title) (fallback_cmd)");
});
