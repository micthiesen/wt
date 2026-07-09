import { describe, expect, test } from "bun:test";

import type { AutomationDef } from "../core/config.ts";
import { StatusKind, type PullRequest } from "../core/types.ts";

import {
  evaluateAutomations,
  fireIdentity,
  type AutomationEvalCtx,
} from "./automation-rules.ts";
import type {
  FieldState,
  StackRowInfo,
  WorktreeRow,
} from "./hooks/useWorktreeRows.ts";

function field<T>(data: T | undefined): FieldState<T> {
  return { data, isStale: false, isFetching: false, isLoading: false, error: null };
}

function makePr(overrides: Partial<PullRequest> = {}): PullRequest {
  return {
    number: 101,
    url: "https://github.com/o/r/pull/101",
    headRefName: "michael/eng-1-x",
    headRefOid: "abc123",
    baseRefName: "main",
    title: "t",
    isDraft: false,
    state: "OPEN",
    checks: "pass",
    failedChecks: [],
    review: "none",
    reviewRequests: 0,
    requestedReviewers: [],
    suggestedReviewers: [],
    rabbit: { state: "none", unresolved: 0 },
    autoMerge: null,
    comments: [],
    unresolvedThreads: 0,
    mergedAt: null,
    closedAt: null,
    ...overrides,
  };
}

function makeRow(
  slug: string,
  overrides: Partial<WorktreeRow> = {},
): WorktreeRow {
  return {
    wt: { slug, path: `/tmp/${slug}`, branch: `michael/${slug}`, isMain: false, stage: slug },
    fields: {
      dirty: field<readonly string[]>([]),
      lock: field(null),
      deploy: field(false),
      merged: field(false),
      gone: field(false),
      sync: field(undefined),
      claude: field(undefined),
      gitActivity: field(undefined),
      conflict: field(undefined),
    },
    status: { kind: StatusKind.Clean, label: "clean" },
    stackedOn: null,
    stack: null,
    archived: false,
    title: slug,
    titleSource: "slug",
    brief: null,
    section: null,
    sectionIsStack: false,
    ...overrides,
  } as WorktreeRow;
}

function stackInfo(stackId: string, ordinal: number): StackRowInfo {
  return {
    stackId,
    ordinal,
    pos: "middle",
    lane: 0,
    depth: ordinal - 1,
    index: ordinal - 1,
    isHolistic: false,
  };
}

function rule(overrides: Partial<AutomationDef>): AutomationDef {
  return {
    id: "r",
    on: "pr.checks.failed",
    run: "fix-ci",
    busy: "queue",
    cooldownMinutes: null,
    settleSeconds: 0,
    ...overrides,
  };
}

const FRESH: AutomationEvalCtx = { githubFresh: true, isPausedSlug: () => false };

describe("pr.checks.failed", () => {
  const r = rule({ id: "fix-ci", on: "pr.checks.failed" });

  test("fires with a head-sha fire key and names the checks", () => {
    const row = makeRow("a", {
      pr: makePr({ checks: "fail", failedChecks: ["typecheck"] }),
    });
    const fires = evaluateAutomations([r], [row], FRESH);
    expect(fires).toHaveLength(1);
    expect(fires[0]!.fireKeys).toEqual(["fix-ci:ci:a:abc123"]);
    expect(fires[0]!.slug).toBe("a");
    expect(fires[0]!.detail).toContain("typecheck");
    expect(fireIdentity(fires[0]!)).toBe("fix-ci|a");
  });

  test("stays silent on stale github data, missing oid, or passing checks", () => {
    const failing = makeRow("a", { pr: makePr({ checks: "fail" }) });
    expect(
      evaluateAutomations([r], [failing], { ...FRESH, githubFresh: false }),
    ).toHaveLength(0);
    const noOid = makeRow("a", {
      pr: makePr({ checks: "fail", headRefOid: undefined }),
    });
    expect(evaluateAutomations([r], [noOid], FRESH)).toHaveLength(0);
    const green = makeRow("a", { pr: makePr({ checks: "pass" }) });
    expect(evaluateAutomations([r], [green], FRESH)).toHaveLength(0);
  });

  test("skips archived, busy, and paused rows", () => {
    const row = makeRow("a", { pr: makePr({ checks: "fail" }) });
    expect(
      evaluateAutomations([r], [{ ...row, archived: true }], FRESH),
    ).toHaveLength(0);
    expect(
      evaluateAutomations(
        [r],
        [{ ...row, status: { kind: StatusKind.Busy, label: "destroying" } }],
        FRESH,
      ),
    ).toHaveLength(0);
    expect(
      evaluateAutomations([r], [row], { ...FRESH, isPausedSlug: (s) => s === "a" }),
    ).toHaveLength(0);
  });
});

describe("rabbit.unresolved", () => {
  test("fires only while CR threads are unresolved", () => {
    const r = rule({ id: "auto-rabbit", on: "rabbit.unresolved", run: "rabbit" });
    const carrots = makeRow("a", {
      pr: makePr({ rabbit: { state: "unresolved", unresolved: 3 } }),
    });
    const fires = evaluateAutomations([r], [carrots], FRESH);
    expect(fires).toHaveLength(1);
    expect(fires[0]!.fireKeys).toEqual(["auto-rabbit:rabbit:a:abc123"]);
    const clean = makeRow("a", {
      pr: makePr({ rabbit: { state: "clean", unresolved: 0 } }),
    });
    expect(evaluateAutomations([r], [clean], FRESH)).toHaveLength(0);
  });
});

describe("wt.merged", () => {
  const r = rule({ id: "auto-clean", on: "wt.merged", run: "builtin:clean" });

  test("fires for a merged non-stack worktree", () => {
    const row = makeRow("a", { pr: makePr({ state: "MERGED" }) });
    const fires = evaluateAutomations([r], [row], FRESH);
    expect(fires).toHaveLength(1);
    expect(fires[0]!.fireKeys).toEqual(["auto-clean:merged:a:101"]);
  });

  test("locally-merged branch fires without github freshness", () => {
    const row = makeRow("a", {
      fields: { ...makeRow("a").fields, merged: field(true) },
      status: { kind: StatusKind.Merged, label: "merged into origin/main" },
    });
    const fires = evaluateAutomations([r], [row], {
      ...FRESH,
      githubFresh: false,
    });
    expect(fires).toHaveLength(1);
    expect(fires[0]!.fireKeys).toEqual(["auto-clean:merged:a:local"]);
  });

  test("never fires for stack slices (restack owns their cleanup)", () => {
    const row = makeRow("a", {
      pr: makePr({ state: "MERGED" }),
      stack: stackInfo("eng-1", 1),
      status: { kind: StatusKind.Merged, label: "merged" },
    });
    expect(evaluateAutomations([r], [row], FRESH)).toHaveLength(0);
  });
});

describe("stack.parent_merged", () => {
  const r = rule({
    id: "auto-restack",
    on: "stack.parent_merged",
    run: "builtin:restack",
  });

  test("fires once per stack with per-parent keys and whole-stack quiesce", () => {
    const merged = makeRow("s1", {
      pr: makePr({ number: 1, state: "MERGED" }),
      stack: stackInfo("eng-9", 1),
      status: { kind: StatusKind.Merged, label: "merged" },
    });
    const open1 = makeRow("s2", {
      pr: makePr({ number: 2 }),
      stack: stackInfo("eng-9", 2),
    });
    const open2 = makeRow("s3", {
      pr: makePr({ number: 3 }),
      stack: stackInfo("eng-9", 3),
    });
    const fires = evaluateAutomations([r], [merged, open1, open2], FRESH);
    expect(fires).toHaveLength(1);
    expect(fires[0]!.stackId).toBe("eng-9");
    expect(fires[0]!.slug).toBe("s2");
    expect(fires[0]!.fireKeys).toEqual(["auto-restack:restack:eng-9:1"]);
    expect(fires[0]!.quiesceSlugs).toEqual(["s1", "s2", "s3"]);
    expect(fireIdentity(fires[0]!)).toBe("auto-restack|eng-9");
  });

  test("a single paused member protects the whole stack from restacks", () => {
    const merged = makeRow("s1", {
      pr: makePr({ number: 1, state: "MERGED" }),
      stack: stackInfo("eng-9", 1),
      status: { kind: StatusKind.Merged, label: "merged" },
    });
    const open = makeRow("s2", {
      pr: makePr({ number: 2 }),
      stack: stackInfo("eng-9", 2),
    });
    const fires = evaluateAutomations([r], [merged, open], {
      ...FRESH,
      isPausedSlug: (s) => s === "s2",
    });
    expect(fires).toHaveLength(0);
  });

  test("silent when nothing merged or nothing open", () => {
    const open = makeRow("s2", { pr: makePr({ number: 2 }), stack: stackInfo("eng-9", 2) });
    expect(evaluateAutomations([r], [open], FRESH)).toHaveLength(0);
    const merged = makeRow("s1", {
      pr: makePr({ number: 1, state: "MERGED" }),
      stack: stackInfo("eng-9", 1),
      status: { kind: StatusKind.Merged, label: "merged" },
    });
    expect(evaluateAutomations([r], [merged], FRESH)).toHaveLength(0);
  });
});
