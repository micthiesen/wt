import { describe, expect, test } from "bun:test";

import type { AutomationFire } from "../automation-rules.ts";
import { isBreakerExemptFire } from "./useAutomations.ts";

const fire = (on: AutomationFire["rule"]["on"]): AutomationFire =>
  ({
    rule: { on, run: "custom-action" },
    frozenVars: null,
  }) as AutomationFire;

describe("automation breaker policy", () => {
  test("a fleet branch move bypasses the breaker regardless of the configured action", () => {
    expect(isBreakerExemptFire(fire("branch.advanced"), false)).toBe(true);
  });

  test("a repeated worktree remediation remains breaker guarded", () => {
    expect(isBreakerExemptFire(fire("pr.checks.failed"), false)).toBe(false);
  });
});
