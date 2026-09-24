import { expect, test } from "bun:test";

import { resolveLanding } from "./landing.ts";

const merged = {
  state: "MERGED" as const,
  baseRefName: "staging",
  mergeCommitOid: "a".repeat(40),
};

test("landing needs a confirmed base merge, not a branch based on staging", () => {
  expect(resolveLanding(false, undefined, "staging", "main", undefined)).toBeNull();
  expect(resolveLanding(false, { ...merged, state: "OPEN" }, "staging", "main", undefined)).toBeNull();
  expect(resolveLanding(false, { ...merged, baseRefName: "feature" }, "staging", "main", undefined)).toBeNull();
  expect(resolveLanding(true, undefined, "staging", "main", undefined)).toBe("base");
});

test("promotion requires the exact staging merge commit on production branch", () => {
  expect(resolveLanding(false, merged, "staging", "main", undefined)).toBe("base");
  expect(resolveLanding(false, merged, "staging", "main", [])).toBe("base");
  expect(resolveLanding(false, merged, "staging", "main", [merged.mergeCommitOid])).toBe("production");
  expect(resolveLanding(false, { ...merged, mergeCommitOid: null }, "staging", "main", [merged.mergeCommitOid])).toBe("base");
});

test("one-branch repos show the configured production shape after landing", () => {
  expect(resolveLanding(true, undefined, "main", "main", undefined)).toBe("production");
  expect(resolveLanding(true, undefined, "main", null, undefined)).toBe("base");
});
