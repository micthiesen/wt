import { describe, expect, test } from "bun:test";

import { evaluateActionRequirements } from "./requirements.ts";
import type { ActionRowState } from "./types.ts";

const rowState = (over: Partial<ActionRowState> = {}): ActionRowState => ({
  slug: "coz-2176-active-louse",
  pr: undefined,
  deployed: false,
  ...over,
});

describe("issue.tracker", () => {
  test("GitHub notes alone cannot run tracker mutations", () => {
    expect(evaluateActionRequirements(["issue.tracker"], rowState({ slug: "gh-123-notes" })).ok).toBe(false);
    expect(evaluateActionRequirements(["issue.tracker"], rowState({ issueId: "GH-123" })).ok).toBe(false);
  });
  test("a slug carrying a tracker id satisfies it", () => {
    expect(evaluateActionRequirements(["issue.tracker"], rowState())).toEqual({ ok: true });
  });

  test("a slug with no tracker id blocks with a reason the picker can print", () => {
    // The live shape that produced this: every worktree on the fleet is
    // keyed to a GitHub issue rather than a tracker task, so `{{issue_id}}`
    // renders empty and the shell action runs with a hole in its argv.
    expect(
      evaluateActionRequirements(["issue.tracker"], rowState({ slug: "codex-delta-reviews" })),
    ).toEqual({ ok: false, reason: "no tracker id (set one with `#`)" });
  });

  test("a stored override satisfies it on a slug that parses to nothing", () => {
    // The whole point of the override: this row is otherwise
    // permanently ineligible, and that population is the common case.
    expect(
      evaluateActionRequirements(
        ["issue.tracker"],
        rowState({ slug: "codex-delta-reviews", issueId: "COZ-2185" }),
      ),
    ).toEqual({ ok: true });
  });

  test("the override wins over an id the slug does carry", () => {
    // Same call, different answer — proving the requirement reads the
    // override rather than merely tolerating it.
    expect(
      evaluateActionRequirements(
        ["issue.tracker"],
        rowState({ slug: "coz-1111-wrong", issueId: "COZ-2185" }),
      ),
    ).toEqual({ ok: true });
  });

  test("a blank override is not an id — it falls back, it does not satisfy", () => {
    // Absence must read as unknown. A stored empty string reaching
    // this as "true" would render `{{issue_id}}` empty again, which is
    // the exact hole the tag exists to close.
    expect(
      evaluateActionRequirements(
        ["issue.tracker"],
        rowState({ slug: "codex-delta-reviews", issueId: "   " }),
      ).ok,
    ).toBe(false);
  });

  test("an id embedded mid-slug still counts", () => {
    expect(
      evaluateActionRequirements(["issue.tracker"], rowState({ slug: "fix-coz-2176-later" })),
    ).toEqual({ ok: true });
  });

  test("the empty slug the row-less manager palette passes is not a tracker id", () => {
    expect(evaluateActionRequirements(["issue.tracker"], rowState({ slug: "" })).ok).toBe(false);
  });

  test("an action that does not ask for it is unaffected", () => {
    expect(
      evaluateActionRequirements([], rowState({ slug: "codex-delta-reviews" })),
    ).toEqual({ ok: true });
  });
});
