import { describe, expect, test } from "bun:test";

import type { HarnessSession } from "../../core/harness/index.ts";

import { computeHarnessSessions } from "./useHarnessSessions.ts";

function codexSession(
  sessionId: string,
  derivedState: HarnessSession["extras"]["derivedState"],
  lastActiveMs = 1_000,
): HarnessSession {
  return {
    displayName: sessionId,
    sessionId,
    tmuxSessionName: "demo-codex",
    lastActiveMs,
    isLive: false,
    extras: {
      managedName: null,
      derivedState,
      queued: 0,
      tailEndedAt: lastActiveMs,
    },
  };
}

describe("computeHarnessSessions single-slot state normalization", () => {
  test("closed Codex session with a clean last turn is idle", () => {
    const result = computeHarnessSessions(
      new Map([["codex", [codexSession("clean", "waiting")]]]),
      new Set(),
      "demo",
      "codex",
      10_000,
    );

    expect(result.f12Target?.extras.derivedState).toBe("idle");
  });

  test("closed Codex session that was mid-turn is abandoned", () => {
    const result = computeHarnessSessions(
      new Map([["codex", [codexSession("mid-turn", "working")]]]),
      new Set(),
      "demo",
      "codex",
      10_000,
    );

    expect(result.f12Target?.extras.derivedState).toBe("abandoned");
  });

  test("live Codex session without a persisted tail is waiting", () => {
    const result = computeHarnessSessions(
      new Map([["codex", [codexSession("fresh", null)]]]),
      new Set(["demo-codex"]),
      "demo",
      "codex",
      10_000,
    );

    expect(result.f12Target?.isLive).toBe(true);
    expect(result.f12Target?.extras.derivedState).toBe("waiting");
  });
});
