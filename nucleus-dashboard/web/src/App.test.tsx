// The sidebar's groups and the redirect of the page's pre-rename address.

import { describe, expect, test } from "vitest";
import { legacyWorkTarget, ROUTES, SIDEBAR_GROUPS } from "./App";

describe("sidebar", () => {
  test("three groups in order: daily, manage, observability", () => {
    expect(SIDEBAR_GROUPS).toEqual(["daily", "manage", "observability"]);
    const labels = (g: string) => ROUTES.filter((r) => r.group === g).map((r) => r.label);
    expect(labels("daily")).toEqual(["dashboard", "chat", "work", "documents", "news"]);
    expect(labels("manage")).toEqual(["reminders", "skills", "vault"]);
    expect(labels("observability")).toEqual(["agents", "tasks", "diary", "usage"]);
  });
});

describe("old links", () => {
  test("/intake?item=n goes to /work?item=n", () => {
    expect(legacyWorkTarget("?item=7")).toBe("/work?item=7");
    expect(legacyWorkTarget("")).toBe("/work");
  });
});
