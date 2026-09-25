import { describe, expect, test } from "vitest";
import {
  canCancel,
  deliveryState,
  formatDuration,
  shortId,
  spanBetween,
  taskStatusKind,
  turnShowsError,
  turnStatusKind,
} from "./tasks";

describe("formatDuration", () => {
  test("seconds under a minute", () => {
    expect(formatDuration(0)).toBe("0s");
    expect(formatDuration(45_900)).toBe("45s");
  });
  test("minutes pad the seconds", () => {
    expect(formatDuration(185_000)).toBe("3m05s");
    expect(formatDuration(59 * 60_000 + 59_000)).toBe("59m59s");
  });
  test("hours pad the minutes", () => {
    expect(formatDuration(3_600_000 + 2 * 60_000 + 30_000)).toBe("1h02m");
  });
  test("days pad the hours", () => {
    expect(formatDuration(2 * 86_400_000 + 3 * 3_600_000)).toBe("2d03h");
  });
  test("negative and non-finite input", () => {
    expect(formatDuration(-1)).toBe("—");
    expect(formatDuration(Number.NaN)).toBe("—");
  });
});

describe("spanBetween", () => {
  const start = "2026-01-01T10:00:00Z";
  test("finished span", () => {
    expect(spanBetween(start, "2026-01-01T10:03:05Z", 0)).toBe("3m05s");
  });
  test("open span is measured to now", () => {
    expect(spanBetween(start, null, Date.parse("2026-01-01T11:02:00Z"))).toBe("1h02m");
  });
  test("not started", () => {
    expect(spanBetween(null, null, Date.now())).toBeNull();
  });
  test("unparseable start", () => {
    expect(spanBetween("not a date", null, Date.now())).toBeNull();
  });
});

describe("shortId", () => {
  test("keeps the first 8 characters", () => {
    expect(shortId("0123456789abcdef")).toBe("01234567");
    expect(shortId("abc")).toBe("abc");
  });
});

describe("canCancel", () => {
  test("queued and running tasks", () => {
    expect(canCancel({ status: "queued" })).toBe(true);
    expect(canCancel({ status: "running" })).toBe(true);
  });
  test("not for finished tasks (a cancel is an immediate transition)", () => {
    for (const status of ["done", "failed", "cancelled", "interrupted"] as const) {
      expect(canCancel({ status })).toBe(false);
    }
  });
});

describe("status kinds", () => {
  test("tasks", () => {
    expect(taskStatusKind("running")).toBe("warn");
    expect(taskStatusKind("done")).toBe("ok");
    expect(taskStatusKind("failed")).toBe("down");
    expect(taskStatusKind("interrupted")).toBe("down");
    expect(taskStatusKind("queued")).toBe("idle");
    expect(taskStatusKind("cancelled")).toBe("idle");
  });
  test("turns", () => {
    expect(turnStatusKind("running")).toBe("warn");
    expect(turnStatusKind("done")).toBe("ok");
    expect(turnStatusKind("silent")).toBe("idle");
    expect(turnStatusKind("failed")).toBe("down");
  });
});

describe("turnShowsError", () => {
  test("only failed or interrupted turns with error text", () => {
    expect(turnShowsError({ status: "failed", error: "timeout" })).toBe(true);
    expect(turnShowsError({ status: "interrupted", error: "restart" })).toBe(true);
    expect(turnShowsError({ status: "failed", error: null })).toBe(false);
    expect(turnShowsError({ status: "done", error: "stale" })).toBe(false);
  });
});

describe("deliveryState", () => {
  const t = (over: Partial<Record<"delivered_at" | "delivery_failed_at" | "delivery_queued_at", string | null>>) => ({
    delivered_at: null,
    delivery_failed_at: null,
    delivery_queued_at: null,
    ...over,
  });
  test("nothing yet", () => expect(deliveryState(t({}))).toBeNull());
  test("queued", () => expect(deliveryState(t({ delivery_queued_at: "x" }))).toBe("queued"));
  test("given up while queued", () =>
    expect(deliveryState(t({ delivery_queued_at: "x", delivery_failed_at: "y" }))).toBe("given-up"));
  test("a late confirmation wins over given up", () =>
    expect(deliveryState(t({ delivery_failed_at: "y", delivered_at: "z" }))).toBe("delivered"));
});
