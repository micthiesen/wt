import { describe, expect, test } from "bun:test";

import { formatPctUsage } from "./usage-badge.tsx";

/** Render clusters the way the badge does, so assertions read as the bar does. */
function render(
  clusters: ReturnType<typeof formatPctUsage>,
  harness: "claude" | "codex" = "claude",
): string {
  return clusters
    .map(
      (c) =>
        `${c.key} ${c.pct}%${harness === "codex" ? " left" : ""}${c.remaining ? ` (${c.remaining})` : ""}`,
    )
    .join(" · ");
}

const NOW = Date.parse("2026-08-08T12:00:00Z");
const IN_5M = "2026-08-08T12:05:00.973683Z";
// Same instant, different microseconds — how the API actually stamps two
// windows that reset together.
const IN_1D13H_A = "2026-08-10T01:00:00.973683Z";
const IN_1D13H_B = "2026-08-10T01:00:00.973897Z";

describe("formatPctUsage", () => {
  test("session, scoped weekly, account-wide weekly, countdown deduped", () => {
    const out = formatPctUsage(
      {
        fiveHour: { utilization: 52, resetsAt: IN_5M },
        sevenDay: { utilization: 20, resetsAt: IN_1D13H_B },
        sevenDayScoped: [
          { utilization: 20, resetsAt: IN_1D13H_A, label: "Fable" },
        ],
        cachedAtMs: NOW,
      },
      NOW,
    );
    expect(render(out)).toBe("5h 52% (5m) · 7f 20% · 7d 20% (1d13h)");
  });

  test("weeklies that reset at different times both keep their countdown", () => {
    const out = formatPctUsage(
      {
        fiveHour: null,
        sevenDay: { utilization: 20, resetsAt: "2026-08-11T01:00:00Z" },
        sevenDayScoped: [
          { utilization: 30, resetsAt: IN_1D13H_A, label: "Fable" },
        ],
        cachedAtMs: NOW,
      },
      NOW,
    );
    expect(render(out)).toBe("7f 30% (1d13h) · 7d 20% (2d13h)");
  });

  test("no account-wide weekly: the other clusters still render", () => {
    const out = formatPctUsage(
      {
        fiveHour: { utilization: 53, resetsAt: IN_5M },
        sevenDay: null,
        sevenDayScoped: [
          { utilization: 21, resetsAt: IN_1D13H_A, label: "Fable" },
        ],
        cachedAtMs: NOW,
      },
      NOW,
    );
    expect(render(out)).toBe("5h 53% (5m) · 7f 21% (1d13h)");
  });

  test("no scoped weekly: reads exactly as it did before scoping existed", () => {
    const out = formatPctUsage(
      {
        fiveHour: { utilization: 43, resetsAt: IN_5M },
        sevenDay: { utilization: 63, resetsAt: IN_1D13H_A },
        sevenDayScoped: [],
        cachedAtMs: NOW,
      },
      NOW,
    );
    expect(render(out)).toBe("5h 43% (5m) · 7d 63% (1d13h)");
  });

  test("several scoped weeklies each get their own initial", () => {
    const out = formatPctUsage(
      {
        fiveHour: null,
        sevenDay: { utilization: 10, resetsAt: IN_1D13H_B },
        sevenDayScoped: [
          { utilization: 74, resetsAt: IN_1D13H_A, label: "Opus" },
          { utilization: 12, resetsAt: IN_1D13H_A, label: "Sonnet" },
        ],
        cachedAtMs: NOW,
      },
      NOW,
    );
    expect(render(out)).toBe("7o 74% · 7s 12% · 7d 10% (1d13h)");
  });

  test("a window past its reset reports 0% with no countdown", () => {
    const out = formatPctUsage(
      {
        fiveHour: { utilization: 90, resetsAt: "2026-08-08T11:00:00Z" },
        sevenDay: null,
        sevenDayScoped: [],
        cachedAtMs: NOW,
      },
      NOW,
    );
    expect(render(out)).toBe("5h 0%");
  });

  test("codex shows remaining allowance, with the weekly label", () => {
    const out = formatPctUsage(
      {
        fiveHour: { utilization: 20, resetsAt: IN_5M },
        sevenDay: { utilization: 62, resetsAt: "2026-08-13T00:00:00Z" },
        planType: "max",
        cachedAtMs: NOW,
      },
      NOW,
      "codex",
    );
    expect(render(out, "codex")).toBe(
      "5h 80% left (5m) · weekly 38% left (4d12h)",
    );
  });

  test("codex can render only the weekly window", () => {
    const out = formatPctUsage(
      {
        fiveHour: null,
        sevenDay: { utilization: 97, resetsAt: IN_1D13H_A },
        planType: "pro",
        cachedAtMs: NOW,
      },
      NOW,
      "codex",
    );
    expect(render(out, "codex")).toBe("weekly 3% left (1d13h)");
  });

  test("expired codex window reports a fresh 100% left", () => {
    const out = formatPctUsage(
      {
        fiveHour: null,
        sevenDay: { utilization: 97, resetsAt: "2026-08-08T11:00:00Z" },
        planType: "pro",
        cachedAtMs: NOW,
      },
      NOW,
      "codex",
    );
    expect(render(out, "codex")).toBe("weekly 100% left");
  });

  test("nothing to show", () => {
    expect(formatPctUsage(null, NOW)).toEqual([]);
    expect(
      formatPctUsage(
        { fiveHour: null, sevenDay: null, sevenDayScoped: [], cachedAtMs: NOW },
        NOW,
      ),
    ).toEqual([]);
  });
});
