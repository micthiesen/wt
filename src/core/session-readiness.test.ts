import { describe, expect, test } from "bun:test";

import { canEnterSessionDuringLock } from "./session-readiness.ts";

describe("canEnterSessionDuringLock", () => {
  test("allows an init lock once the checkout exists", () => {
    expect(canEnterSessionDuringLock({ op: "init" }, true)).toBe(true);
  });

  test("blocks init before the checkout exists", () => {
    expect(canEnterSessionDuringLock({ op: "init" }, false)).toBe(false);
  });

  test("blocks destructive and unknown operations", () => {
    expect(canEnterSessionDuringLock({ op: "remove" }, true)).toBe(false);
    expect(canEnterSessionDuringLock({ op: "restack" }, true)).toBe(false);
    expect(canEnterSessionDuringLock({ op: "" }, true)).toBe(false);
  });

  test("allows an unlocked checkout", () => {
    expect(canEnterSessionDuringLock(null, true)).toBe(true);
  });
});
