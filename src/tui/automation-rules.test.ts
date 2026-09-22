import { describe, expect, test } from "bun:test";

import { config } from "../core/config.ts";
import { evaluateActionRequirements } from "../core/actions/requirements.ts";
import type { AutomationDef } from "../core/config.ts";
import { StatusKind, type PullRequest } from "../core/types.ts";

import {
  evaluateAutomations,
  fireIdentity,
  FLEET_SLUG,
  type AutomationEvalCtx,
  type FireAudience,
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
    reviewBot: { state: "none", unresolved: 0 },
    autoMerge: null,
    comments: [],
    unresolvedThreads: 0,
    unresolvedThreadsTotal: 0,
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
    lane: 0,
    depth: ordinal - 1,
    index: ordinal - 1,
  };
}

function rule(overrides: Partial<AutomationDef>): AutomationDef {
  return {
    id: "r",
    on: "pr.checks.failed",
    run: "fix-ci",
    busy: "queue",
    cooldownMinutes: null,
    afterDays: 2,
    settleSeconds: 0,
  branch: null,
    ...overrides,
  };
}

const FRESH: AutomationEvalCtx = {
  githubFresh: true,
  isPausedSlug: () => false,
  audienceOf: () => null,
  externalOf: () => false,
  varsFor: () => ({}) as never,
      branchTips: new Map(),
  nowMs: Date.parse("2026-08-20T12:00:00Z"),
};

describe("wt.created", () => {
  const r = rule({ id: "started", on: "wt.created" });
  const createdAt = "2026-09-22T12:00:00.000Z";
  test("only explicit successful creations fire, once per checkout identity", () => {
    expect(evaluateAutomations([r], [makeRow("legacy")], FRESH)).toEqual([]);
    const fires = evaluateAutomations([r], [makeRow("fresh", { createdAt })], FRESH);
    expect(fires[0]?.fireKeys).toEqual([`started:created:fresh:${createdAt}`]);
    expect(fires[0]?.quiesceSlugs).toEqual(["fresh"]);
    const later = evaluateAutomations([r], [makeRow("fresh", { createdAt: "2026-09-23T00:00:00.000Z" })], FRESH);
    expect(later[0]?.fireKeys).not.toEqual(fires[0]?.fireKeys);
  });
  test("external creation bookkeeping bypasses busy harness but honors pause and landing", () => {
    const row = makeRow("fresh", { createdAt });
    expect(evaluateAutomations([r], [row], { ...FRESH, externalOf: () => true })[0]?.quiesceSlugs).toEqual([]);
    expect(evaluateAutomations([r], [row], { ...FRESH, isPausedSlug: () => true })).toEqual([]);
    expect(evaluateAutomations([r], [makeRow("fresh", { createdAt, pr: makePr({ state: "MERGED" }) })], FRESH)).toEqual([]);
    row.fields.merged = field(true);
    expect(evaluateAutomations([r], [row], FRESH)).toEqual([]);
  });
});

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

describe("review_bot.unresolved", () => {
  test("fires only while bot findings are unresolved", () => {
    const r = rule({ id: "auto-rabbit", on: "review_bot.unresolved", run: "rabbit" });
    const unresolved = makeRow("a", {
      pr: makePr({ reviewBot: { state: "unresolved", unresolved: 3 } }),
    });
    const fires = evaluateAutomations([r], [unresolved], FRESH);
    expect(fires).toHaveLength(1);
    // ":rabbit:" is frozen for on-disk ledger continuity (see the
    // review_bot.unresolved case in automation-rules.ts).
    expect(fires[0]!.fireKeys).toEqual(["auto-rabbit:rabbit:a:abc123"]);
    const clean = makeRow("a", {
      pr: makePr({ reviewBot: { state: "clean", unresolved: 0 } }),
    });
    expect(evaluateAutomations([r], [clean], FRESH)).toHaveLength(0);
  });

  test("treats a cache-restored PR without the reviewBot field as none", () => {
    const r = rule({ id: "auto-rabbit", on: "review_bot.unresolved", run: "rabbit" });
    const pr = makePr({});
    // Entries persisted before the rabbit → reviewBot rename lack the
    // field entirely; the trigger must read that as "no bot state".
    delete (pr as { reviewBot?: unknown }).reviewBot;
    const row = makeRow("a", { pr });
    expect(evaluateAutomations([r], [row], FRESH)).toHaveLength(0);
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

describe("wt.merged → builtin:close-issue", () => {
  const r = rule({ id: "close-gh", on: "wt.merged", run: "builtin:close-issue" });

  test("fires only when an issue is attached, with an empty quiesce set", () => {
    const bare = makeRow("a", { pr: makePr({ state: "MERGED" }) });
    expect(evaluateAutomations([r], [bare], FRESH)).toHaveLength(0);
    const attached = makeRow("a", {
      pr: makePr({ state: "MERGED" }),
      githubIssue: 970,
    });
    const fires = evaluateAutomations([r], [attached], FRESH);
    expect(fires).toHaveLength(1);
    expect(fires[0]!.fireKeys).toEqual(["close-gh:merged:a:101"]);
    // Empty on purpose: the close never touches the worktree, and the
    // empty set keeps it out of the dispatch loop's slug contention so
    // a racing clean/restack can't starve it into a superseded drop.
    expect(fires[0]!.quiesceSlugs).toEqual([]);
    // Frozen into the fire — delivery must not depend on the row (or
    // its wtstate entry) surviving until dispatch.
    expect(fires[0]!.closeIssue).toBe(970);
    expect(fires[0]!.detail).toContain("#970");
  });

  test("a GH-<n> primary slug id counts as the attached issue", () => {
    const row = makeRow("gh-88-fix-tabs", { pr: makePr({ state: "MERGED" }) });
    const fires = evaluateAutomations([r], [row], FRESH);
    expect(fires).toHaveLength(1);
    expect(fires[0]!.closeIssue).toBe(88);
  });

  test("unlike other wt.merged rules, fires for stack members", () => {
    const row = makeRow("a", {
      pr: makePr({ state: "MERGED" }),
      githubIssue: 970,
      stack: stackInfo("eng-1", 1),
      status: { kind: StatusKind.Merged, label: "merged" },
    });
    expect(evaluateAutomations([r], [row], FRESH)).toHaveLength(1);
  });
});

describe("wt.merged → an external shell action", () => {
  // The tracker transition (`issue-in-review` on this fleet): a config
  // shell action declared `external = true`. Same class as the two
  // post-merge builtins, and treated as the opposite until every fire
  // for three days was dropped as superseded.
  const r = rule({ id: "issue-in-review-on-merge", on: "wt.merged", run: "issue-in-review" });
  const EXTERNAL: AutomationEvalCtx = {
    ...FRESH,
    externalOf: () => true,
    varsFor: (_rule, row) => ({ slug: row.wt.slug, issue_id: "COZ-2185" }) as never,
  };

  test("carries an empty quiesce set and its vars frozen", () => {
    const row = makeRow("a", { pr: makePr({ state: "MERGED" }) });
    const fires = evaluateAutomations([r], [row], EXTERNAL);
    expect(fires).toHaveLength(1);
    // Empty for the same reason close-issue's is: it touches a ticket
    // tracker, never the checkout. A non-empty set is what queued it
    // behind the row's own lifetime and lost the race to the `c` sweep.
    expect(fires[0]!.quiesceSlugs).toEqual([]);
    // Non-null `frozenVars` is ALSO the dispatcher's marker for "this
    // outlives its row", so an empty map here would silently restore
    // the old behaviour.
    expect(fires[0]!.frozenVars).toEqual({ slug: "a", issue_id: "COZ-2185" });
    // Dispatch no longer needs the row (which may have been deleted).
    const evidence = { slug: "a", issueId: "COZ-2185", pr: fires[0]!.frozenPr, deployed: false };
    expect(evaluateActionRequirements(["pr", "issue.tracker"], evidence)).toEqual({ ok: true });
    expect(evaluateActionRequirements(["pr.ready"], evidence)).toEqual({ ok: false, reason: "PR not open" });
  });

  test("fires for stack members, unlike a worktree-touching merge rule", () => {
    const row = makeRow("a", {
      pr: makePr({ state: "MERGED" }),
      stack: stackInfo("eng-1", 1),
      status: { kind: StatusKind.Merged, label: "merged" },
    });
    expect(evaluateAutomations([r], [row], EXTERNAL)).toHaveLength(1);
  });

  test("a NON-external action on the same trigger keeps the row semantics", () => {
    // The guard against over-reach: `external` is what earns the
    // exemption, not the trigger. An ordinary shell action on
    // wt.merged must still quiesce on its slug and skip stack members.
    const row = makeRow("a", { pr: makePr({ state: "MERGED" }) });
    const fires = evaluateAutomations([r], [row], FRESH);
    expect(fires).toHaveLength(1);
    expect(fires[0]!.quiesceSlugs).toEqual(["a"]);
    expect(fires[0]!.frozenVars).toBeNull();
  });
});

describe("status.* (work-status triggers)", () => {
  const STALE: AutomationEvalCtx = {
    githubFresh: false,
    isPausedSlug: () => false,
    audienceOf: () => null,
    externalOf: () => false,
    varsFor: () => ({}) as never,
      branchTips: new Map(),
    nowMs: Date.parse("2026-08-20T12:00:00Z"),
  };

  test("fires on the matching asserted state, keyed by assertion time", () => {
    const r = rule({ id: "ping", on: "status.needs_human" });
    const row = makeRow("a", {
      work: {
        state: "needs-human",
        note: "log me into the dev env",
        at: "2026-08-08T10:00:00Z",
      },
    });
    // Local state — must fire even before any github fetch.
    const fires = evaluateAutomations([r], [row], STALE);
    expect(fires).toHaveLength(1);
    expect(fires[0]!.fireKeys).toEqual(["ping:work:a:2026-08-08T10:00:00Z"]);
    expect(fires[0]!.detail).toContain("log me into the dev env");
  });

  test("re-asserting produces a new fire key; other states don't fire", () => {
    const r = rule({ id: "ping", on: "status.ready" });
    const first = makeRow("a", {
      work: { state: "ready", risk: "medium", note: "resync", at: "t1" },
    });
    const second = makeRow("a", {
      work: { state: "ready", risk: "medium", note: "resync", at: "t2" },
    });
    const k1 = evaluateAutomations([r], [first], FRESH)[0]!.fireKeys[0];
    const k2 = evaluateAutomations([r], [second], FRESH)[0]!.fireKeys[0];
    expect(k1).not.toBe(k2);
    const wrongState = makeRow("a", { work: { state: "working", at: "t3" } });
    expect(evaluateAutomations([r], [wrongState], FRESH)).toHaveLength(0);
    expect(evaluateAutomations([r], [makeRow("a")], FRESH)).toHaveLength(0);
  });

  test("needs_testing fires on its own state only", () => {
    const r = rule({ id: "nudge", on: "status.needs_testing" });
    const row = makeRow("a", {
      work: { state: "needs-testing", note: "verify email copy", at: "t" },
    });
    const fires = evaluateAutomations([r], [row], STALE);
    expect(fires).toHaveLength(1);
    expect(fires[0]!.detail).toBe("needs-testing — verify email copy");
    const other = makeRow("a", { work: { state: "needs-human", note: "x", at: "t" } });
    expect(evaluateAutomations([r], [other], STALE)).toHaveLength(0);
  });

  // The manager's last triage step is sharpening the needs-human note
  // it was briefed about — a re-assertion, so a new `at`, so a new fire
  // key, so another briefing quoting its own words back at it. Observed
  // three times for one slug before the guard existed.
  describe("a briefing never echoes its own audience's write", () => {
    const brief: AutomationEvalCtx = { ...STALE, audienceOf: () => "manager" };
    const r = rule({ id: "brief", on: "status.needs_human" });
    const escalated = (by: string | undefined) =>
      makeRow("a", {
        work: { state: "needs-human", note: "log me in", at: "t1", ...(by ? { by } : {}) },
      });

    test("the manager's own assertion does not brief the manager", () => {
      expect(evaluateAutomations([r], [escalated("manager")], brief)).toHaveLength(0);
    });

    test("the worker's assertion still does", () => {
      expect(evaluateAutomations([r], [escalated("a")], brief)).toHaveLength(1);
      // Pre-`by` records, and the human at the `u` picker: unattributed
      // is not "the audience", so it must still fire. A guard that keys
      // on stored state fails OPEN on rows that have none.
      expect(evaluateAutomations([r], [escalated(undefined)], brief)).toHaveLength(1);
    });

    test("a rule aimed at the worktree's own session is the same loop", () => {
      const own: AutomationEvalCtx = { ...STALE, audienceOf: () => "session" };
      expect(evaluateAutomations([r], [escalated("a")], own)).toHaveLength(0);
      expect(evaluateAutomations([r], [escalated("manager")], own)).toHaveLength(1);
    });

    test("an audience-less run (notify, clean, headless) is never suppressed", () => {
      expect(evaluateAutomations([r], [escalated("manager")], STALE)).toHaveLength(1);
    });
  });

  test("ready detail carries the risk", () => {
    const r = rule({ id: "ping", on: "status.ready" });
    const row = makeRow("a", {
      work: { state: "ready", risk: "high", note: "not reasonably testable", at: "t" },
    });
    expect(evaluateAutomations([r], [row], FRESH)[0]!.detail).toBe(
      "ready (risk: high) — not reasonably testable",
    );
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

  test("stack-on-stack: fires when the external parent's PR merged", () => {
    // Stack A's only slice merged — A itself has nothing open, so A
    // must NOT fire. Stack B's root is based on A's branch and must.
    const aTip = makeRow("a-tip", {
      pr: makePr({ number: 10, state: "MERGED" }),
      stack: stackInfo("eng-a", 1),
      status: { kind: StatusKind.Merged, label: "merged" },
    });
    const bRoot = makeRow("b-root", {
      pr: makePr({ number: 20 }),
      stack: stackInfo("eng-b", 1),
      stackedOn: {
        slug: "a-tip",
        branch: aTip.wt.branch,
        diffBase: aTip.wt.branch,
      },
    });
    const fires = evaluateAutomations([r], [aTip, bRoot], FRESH);
    expect(fires).toHaveLength(1);
    expect(fires[0]!.stackId).toBe("eng-b");
    expect(fires[0]!.fireKeys).toEqual(["auto-restack:restack:eng-b:ext:10"]);
    expect(fires[0]!.quiesceSlugs).toEqual(["b-root", "a-tip"]);
  });

  test("stack-on-stack: silent while the external parent is still open", () => {
    const aTip = makeRow("a-tip", {
      pr: makePr({ number: 10 }),
      stack: stackInfo("eng-a", 1),
    });
    const bRoot = makeRow("b-root", {
      pr: makePr({ number: 20 }),
      stack: stackInfo("eng-b", 1),
      stackedOn: {
        slug: "a-tip",
        branch: aTip.wt.branch,
        diffBase: aTip.wt.branch,
      },
    });
    expect(evaluateAutomations([r], [aTip, bRoot], FRESH)).toHaveLength(0);
  });

  test("stack-on-stack: fires once when the external parent has no worktree left", () => {
    const bRoot = makeRow("b-root", {
      pr: makePr({ number: 20 }),
      stack: stackInfo("eng-b", 1),
      stackedOn: {
        slug: null,
        branch: "michael/a-tip",
        diffBase: "michael/a-tip",
      },
    });
    const fires = evaluateAutomations([r], [bRoot], FRESH);
    expect(fires).toHaveLength(1);
    expect(fires[0]!.fireKeys).toEqual([
      "auto-restack:restack:eng-b:extgone:michael/a-tip",
    ]);
    expect(fires[0]!.quiesceSlugs).toEqual(["b-root"]);
  });

  test("stack-on-stack: a paused external parent blocks the boundary fire", () => {
    const aTip = makeRow("a-tip", {
      pr: makePr({ number: 10, state: "MERGED" }),
      stack: stackInfo("eng-a", 1),
      status: { kind: StatusKind.Merged, label: "merged" },
    });
    const bRoot = makeRow("b-root", {
      pr: makePr({ number: 20 }),
      stack: stackInfo("eng-b", 1),
      stackedOn: {
        slug: "a-tip",
        branch: aTip.wt.branch,
        diffBase: aTip.wt.branch,
      },
    });
    const fires = evaluateAutomations([r], [aTip, bRoot], {
      ...FRESH,
      isPausedSlug: (s) => s === "a-tip",
    });
    expect(fires).toHaveLength(0);
  });
});

describe("wt.merged → builtin:delete-branch", () => {
  const r = rule({ id: "rm-branch", on: "wt.merged", run: "builtin:delete-branch" });

  test("freezes the branch onto the fire with an empty quiesce set", () => {
    const row = makeRow("a", { pr: makePr({ state: "MERGED" }) });
    const fires = evaluateAutomations([r], [row], FRESH);
    expect(fires).toHaveLength(1);
    // Frozen for the same reason close-issue freezes its number, and it
    // matters more here: a live re-read at delivery could resolve a
    // recreated slug to a branch that has not landed, and a deleted ref
    // is the one thing wt cannot undo.
    expect(fires[0]!.deleteBranch).toBe("michael/a");
    expect(fires[0]!.closeIssue).toBeNull();
    // Empty on purpose: this touches GitHub, never the checkout, so a
    // racing clean/restack must not be able to starve it.
    expect(fires[0]!.quiesceSlugs).toEqual([]);
    expect(fires[0]!.detail).toContain("michael/a");
  });

  test("does not need an attached issue (unlike close-issue)", () => {
    const row = makeRow("a", { pr: makePr({ state: "MERGED" }) });
    expect(evaluateAutomations([r], [row], FRESH)[0]!.deleteBranch).toBe("michael/a");
  });

  test("evaluates stacked members too", () => {
    // A merged stack member is normally skipped so a clean can't race a
    // whole-stack restack, but deleting a remote ref touches no
    // worktree — and skipping would miss every stacked landing, which
    // is most of them on a stacked fleet.
    const row = makeRow("eng-1", {
      pr: makePr({ state: "MERGED" }),
      stack: stackInfo("eng-1", 1),
    });
    expect(evaluateAutomations([r], [row], FRESH)).toHaveLength(1);
  });

  test("never fires for a branch that has not landed", () => {
    const open = makeRow("a", { pr: makePr({ state: "OPEN" }) });
    expect(evaluateAutomations([r], [open], FRESH)).toHaveLength(0);
  });

  test("refuses the trunk branch even if a row somehow carries it", () => {
    // Defence in depth, not a live worry: no worktree branch is ever the
    // trunk. It is here because this is the one mutation in the codebase
    // whose blast radius is the repo's mainline rather than a retry.
    const row = makeRow("a", { pr: makePr({ state: "MERGED" }) });
    const onTrunk = {
      ...row,
      wt: { ...row.wt, branch: config.branch.base },
    };
    expect(evaluateAutomations([r], [onTrunk], FRESH)).toHaveLength(0);
  });

  test("shares the merged fire key shape, so one landing fires once", () => {
    const row = makeRow("a", { pr: makePr({ state: "MERGED" }) });
    expect(evaluateAutomations([r], [row], FRESH)[0]!.fireKeys).toEqual([
      "rm-branch:merged:a:101",
    ]);
  });
});

describe("status.verification_overdue", () => {
  const r = rule({ id: "nag", on: "status.verification_overdue", afterDays: 2 });
  const NOW = Date.parse("2026-08-20T12:00:00Z");
  const ctx: AutomationEvalCtx = { ...FRESH, nowMs: NOW };
  const owed = (at: string) =>
    ({ state: "ready", at, verifyAfterMerge: "connect gcal" }) as WorktreeRow["work"];
  const merged = { kind: StatusKind.Merged, label: "merged" } as WorktreeRow["status"];

  test("fires on a landed row whose check has aged out", () => {
    const row = makeRow("a", { status: merged, work: owed("2026-08-17T12:00:00Z") });
    const fires = evaluateAutomations([r], [row], ctx);
    expect(fires).toHaveLength(1);
    expect(fires[0]!.detail).toContain("connect gcal");
  });

  // The one repeating fire key in the engine: the instance is "this
  // obligation, today", so two passes on the same day share a key and
  // the ledger fires once — but tomorrow is a new instance.
  test("its key is stable within a day and moves the next", () => {
    const row = makeRow("a", { status: merged, work: owed("2026-08-01T12:00:00Z") });
    const morning = evaluateAutomations([r], [row], { ...ctx, nowMs: NOW })[0]!.fireKeys[0];
    const evening = evaluateAutomations([r], [row], {
      ...ctx,
      nowMs: NOW + 6 * 3600_000,
    })[0]!.fireKeys[0];
    const tomorrow = evaluateAutomations([r], [row], {
      ...ctx,
      nowMs: NOW + 24 * 3600_000,
    })[0]!.fireKeys[0];
    expect(evening).toBe(morning!);
    expect(tomorrow).not.toBe(morning!);
  });

  test("silent inside the window", () => {
    const row = makeRow("a", { status: merged, work: owed("2026-08-19T12:00:00Z") });
    expect(evaluateAutomations([r], [row], ctx)).toHaveLength(0);
  });

  // The property that keeps this from becoming a second --blocked-on.
  test("never fires before the branch lands", () => {
    const row = makeRow("a", { work: owed("2026-01-01T12:00:00Z") });
    expect(evaluateAutomations([r], [row], ctx)).toHaveLength(0);
  });

  test("verified ends it", () => {
    const row = makeRow("a", {
      status: merged,
      work: { ...owed("2026-08-01T12:00:00Z")!, state: "verified" } as WorktreeRow["work"],
    });
    expect(evaluateAutomations([r], [row], ctx)).toHaveLength(0);
  });

  // `rowHasLanded` also accepts a MERGED pr, and that leg is
  // github-derived: a boot-stale cache must not nag about a merge it
  // has not confirmed this session.
  test("the PR-merged leg waits for fresh github data", () => {
    const row = makeRow("a", {
      work: owed("2026-08-01T12:00:00Z"),
      pr: makePr({ state: "MERGED" }),
    });
    expect(
      evaluateAutomations([r], [row], { ...ctx, githubFresh: false }),
    ).toHaveLength(0);
    expect(evaluateAutomations([r], [row], ctx)).toHaveLength(1);
  });
});

describe("branch.advanced", () => {
  const r = rule({ id: "release", on: "branch.advanced", branch: "main" });
  const ctxWith = (tips: [string, { now: string; seen: string | null }][]) => ({
    githubFresh: true,
    isPausedSlug: () => false,
    audienceOf: () => null as FireAudience,
    externalOf: () => false,
    varsFor: () => ({}) as never,
    branchTips: new Map(tips),
    nowMs: 0,
  });

  // The whole reason the trigger is fleet-level: by release time the
  // rows whose work is in the range have been swept, so it must fire
  // with no rows at all.
  test("fires with an empty board", () => {
    const fires = evaluateAutomations([r], [], ctxWith([["main", { now: "bbb", seen: "aaa" }]]));
    expect(fires).toHaveLength(1);
    expect(fires[0]!.slug).toBe(FLEET_SLUG);
    expect(fires[0]!.branchRange).toEqual({ branch: "main", from: "aaa", to: "bbb" });
  });

  // The dangerous case. An absent watermark is "not seen yet", never
  // "everything up to here" — the latter would fire once across the
  // branch's entire history, which for the run this exists for means
  // marking every issue ever shipped.
  test("first sight fires NOTHING", () => {
    expect(
      evaluateAutomations([r], [], ctxWith([["main", { now: "bbb", seen: null }]])),
    ).toEqual([]);
  });

  test("an unmoved tip fires nothing", () => {
    expect(
      evaluateAutomations([r], [], ctxWith([["main", { now: "aaa", seen: "aaa" }]])),
    ).toEqual([]);
  });

  test("an unresolvable branch fires nothing", () => {
    expect(evaluateAutomations([r], [], ctxWith([]))).toEqual([]);
  });

  // Keyed on the DESTINATION, so a branch that moves again while the
  // first fire is still pending gets its own key rather than being
  // swallowed as a duplicate.
  test("the fire key carries the destination sha", () => {
    const a = evaluateAutomations([r], [], ctxWith([["main", { now: "bbb", seen: "aaa" }]]));
    const b = evaluateAutomations([r], [], ctxWith([["main", { now: "ccc", seen: "aaa" }]]));
    expect(a[0]!.fireKeys).not.toEqual(b[0]!.fireKeys);
  });

  // Nothing to wait on: the run touches no checkout, and quiescing on a
  // fleet would mean a busy fleet never releases.
  test("waits on no worktree", () => {
    const fires = evaluateAutomations([r], [], ctxWith([["main", { now: "bbb", seen: "aaa" }]]));
    expect(fires[0]!.quiesceSlugs).toEqual([]);
  });
});

describe("wt.merged — delete-branch evidence", () => {
  const r = rule({ id: "del", on: "wt.merged", run: "builtin:delete-branch" });

  test("a PR-merged fire names the PR and freezes its number", () => {
    const row = makeRow("a", {
      pr: makePr({ number: 1618, state: "MERGED" }),
      status: { kind: StatusKind.Merged, label: "merged" },
    });
    const [fire] = evaluateAutomations([r], [row], FRESH);
    expect(fire?.deleteBranch).toBe("michael/a");
    expect(fire?.deleteBranchPr).toBe(1618);
    expect(fire?.detail).toContain("#1618 merged");
  });

  test("a LOCAL-merged fire does not claim the PR merged", () => {
    // The incident: the merged verdict came from the local leg (a
    // cached answer computed for a previous branch) while the row
    // carried a fresh, OPEN PR. The line said "#1631 merged", which is
    // wt asserting something false in its own audit trail — and it is
    // what sent two readers looking for whoever closed the PR.
    const row = makeRow("a", {
      pr: makePr({ number: 1631, state: "OPEN" }),
      // What the stale slug-keyed cache actually produced: the derived
      // status says merged (computed for the PREVIOUS branch) while the
      // PR on the row is fresh, current, and open.
      status: { kind: StatusKind.Merged, label: "merged" },
      fields: { ...makeRow("a").fields, merged: field(true) },
    });
    const [fire] = evaluateAutomations([r], [row], FRESH);
    expect(fire).toBeDefined();
    expect(fire?.detail).not.toContain("#1631 merged");
    expect(fire?.detail).toContain("not observed merged");
    // Frozen so the dispatch can confirm the claim before deleting.
    expect(fire?.deleteBranchPr).toBe(1631);
  });

  test("no PR at all freezes null, so the dispatch may proceed on local evidence", () => {
    const row = makeRow("a", {
      status: { kind: StatusKind.Merged, label: "merged" },
      fields: { ...makeRow("a").fields, merged: field(true) },
    });
    const [fire] = evaluateAutomations([r], [row], FRESH);
    expect(fire?.deleteBranchPr).toBeNull();
    expect(fire?.detail).toBe("branch landed on trunk — deleting remote branch michael/a");
  });
});
