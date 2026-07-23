import { describe, expect, test } from "bun:test";

import {
  compareTasks,
  deriveTaskState,
  TASK_BUCKET_ORDER,
  type TaskBucket,
  type TaskManual,
  type TaskPrSignals,
  type TaskSignals,
} from "./task-state.ts";

/** Baseline "nothing going on" signals — every table test overrides
 *  only the fields relevant to the rung it's exercising, so a rung's
 *  test reads as a diff against "totally quiet worktree" rather than
 *  restating every field. */
function signals(overrides: Partial<TaskSignals> = {}): TaskSignals {
  return {
    sessionState: null,
    turnEndedAt: null,
    lastFocusedAt: null,
    busyLock: false,
    actionRunning: false,
    conflict: false,
    midRebase: false,
    dirty: false,
    mergedOrGone: false,
    pr: null,
    lastActivityMs: 0,
    ...overrides,
  };
}

function pr(overrides: Partial<TaskPrSignals> = {}): TaskPrSignals {
  return {
    state: "OPEN",
    isDraft: false,
    checks: "none",
    reviewDecision: null,
    autoMergeArmed: false,
    inMergeQueue: false,
    ...overrides,
  };
}

const NOT_PINNED_NOT_SNOOZED: TaskManual = { pinned: false, snoozedBucket: null };

function manual(overrides: Partial<TaskManual> = {}): TaskManual {
  return { ...NOT_PINNED_NOT_SNOOZED, ...overrides };
}

describe("deriveTaskState — one case per precedence rung", () => {
  test("1. done: mergedOrGone", () => {
    const s = deriveTaskState(signals({ mergedOrGone: true }), manual());
    expect(s).toEqual({ bucket: "done", reason: "merged", snoozed: false });
  });

  test("1. done: pr.state MERGED", () => {
    const s = deriveTaskState(signals({ pr: pr({ state: "MERGED" }) }), manual());
    expect(s.bucket).toBe("done");
    expect(s.reason).toBe("merged");
  });

  test("2a. needs-you: sessionState asking", () => {
    const s = deriveTaskState(signals({ sessionState: "asking" }), manual());
    expect(s.bucket).toBe("needs-you");
    expect(s.reason).toBe("agent is asking");
  });

  test("2b. needs-you: midRebase", () => {
    const s = deriveTaskState(signals({ midRebase: true }), manual());
    expect(s.bucket).toBe("needs-you");
    expect(s.reason).toBe("conflict mid-rebase");
  });

  test("2c. needs-you: conflict", () => {
    const s = deriveTaskState(signals({ conflict: true }), manual());
    expect(s.bucket).toBe("needs-you");
    expect(s.reason).toBe("merge conflict with base");
  });

  test("2d. needs-you: open PR (draft) with failing checks", () => {
    const s = deriveTaskState(
      signals({ pr: pr({ isDraft: true, checks: "failing" }) }),
      manual(),
    );
    expect(s.bucket).toBe("needs-you");
    expect(s.reason).toBe("checks failing");
  });

  test("2e. needs-you: open non-draft PR with changes requested", () => {
    const s = deriveTaskState(
      signals({ pr: pr({ reviewDecision: "CHANGES_REQUESTED" }) }),
      manual(),
    );
    expect(s.bucket).toBe("needs-you");
    expect(s.reason).toBe("changes requested");
  });

  test("2e. draft PR with changes requested does NOT hit needs-you (draft guard)", () => {
    const s = deriveTaskState(
      signals({ pr: pr({ isDraft: true, reviewDecision: "CHANGES_REQUESTED" }) }),
      manual(),
    );
    expect(s.bucket).not.toBe("needs-you");
  });

  test("3a. working: busyLock", () => {
    const s = deriveTaskState(signals({ busyLock: true }), manual());
    expect(s.bucket).toBe("working");
    expect(s.reason).toBe("worktree busy");
  });

  test("3b. working: actionRunning", () => {
    const s = deriveTaskState(signals({ actionRunning: true }), manual());
    expect(s.bucket).toBe("working");
    expect(s.reason).toBe("action running");
  });

  test("3c. working: sessionState working", () => {
    const s = deriveTaskState(signals({ sessionState: "working" }), manual());
    expect(s.bucket).toBe("working");
    expect(s.reason).toBe("agent working");
  });

  test("3c. working: sessionState polling", () => {
    const s = deriveTaskState(signals({ sessionState: "polling" }), manual());
    expect(s.bucket).toBe("working");
    expect(s.reason).toBe("agent polling a task");
  });

  test("3d. working: auto-merge armed with pending checks", () => {
    const s = deriveTaskState(
      signals({ pr: pr({ autoMergeArmed: true, checks: "pending" }) }),
      manual(),
    );
    expect(s.bucket).toBe("working");
    expect(s.reason).toBe("auto-merge armed · ci running");
  });

  test("4. review-output: turn ended, never focused", () => {
    const s = deriveTaskState(
      signals({ sessionState: "waiting", turnEndedAt: 100, lastFocusedAt: null }),
      manual(),
    );
    expect(s.bucket).toBe("review-output");
    expect(s.reason).toBe("unreviewed agent output");
  });

  test("4. review-output: turn ended after last focus", () => {
    const s = deriveTaskState(
      signals({ sessionState: "waiting", turnEndedAt: 200, lastFocusedAt: 100 }),
      manual(),
    );
    expect(s.bucket).toBe("review-output");
  });

  test("4. unread bit: focused AFTER the turn end falls through (not review-output)", () => {
    const s = deriveTaskState(
      signals({ sessionState: "waiting", turnEndedAt: 100, lastFocusedAt: 200 }),
      manual(),
    );
    expect(s.bucket).not.toBe("review-output");
    expect(s.bucket).toBe("idle");
  });

  test("5. ready: approved + passing checks", () => {
    const s = deriveTaskState(
      signals({ pr: pr({ reviewDecision: "APPROVED", checks: "passing" }) }),
      manual(),
    );
    expect(s.bucket).toBe("ready");
    expect(s.reason).toBe("approved · green");
  });

  test("5. ready: approved + no checks configured", () => {
    const s = deriveTaskState(
      signals({ pr: pr({ reviewDecision: "APPROVED", checks: "none" }) }),
      manual(),
    );
    expect(s.bucket).toBe("ready");
  });

  test("6a. waiting: in merge queue", () => {
    const s = deriveTaskState(signals({ pr: pr({ inMergeQueue: true }) }), manual());
    expect(s.bucket).toBe("waiting");
    expect(s.reason).toBe("in merge queue");
  });

  test("6b. waiting: pending checks on open non-draft PR", () => {
    const s = deriveTaskState(signals({ pr: pr({ checks: "pending" }) }), manual());
    expect(s.bucket).toBe("waiting");
    expect(s.reason).toBe("ci running");
  });

  test("6c. waiting: awaiting review (null decision)", () => {
    const s = deriveTaskState(signals({ pr: pr({ reviewDecision: null }) }), manual());
    expect(s.bucket).toBe("waiting");
    expect(s.reason).toBe("awaiting review");
  });

  test("6c. waiting: awaiting review (REVIEW_REQUIRED decision)", () => {
    const s = deriveTaskState(
      signals({ pr: pr({ reviewDecision: "REVIEW_REQUIRED" }) }),
      manual(),
    );
    expect(s.bucket).toBe("waiting");
    expect(s.reason).toBe("awaiting review");
  });

  test("7. idle: quiet worktree, nothing pending", () => {
    const s = deriveTaskState(signals(), manual());
    expect(s.bucket).toBe("idle");
    expect(s.reason).toBe("idle");
  });

  test("7. idle: dirty worktree", () => {
    const s = deriveTaskState(signals({ dirty: true }), manual());
    expect(s.bucket).toBe("idle");
    expect(s.reason).toBe("dirty");
  });

  test("7. idle: abandoned session with output already seen", () => {
    const s = deriveTaskState(
      signals({ sessionState: "abandoned", turnEndedAt: 100, lastFocusedAt: 200 }),
      manual(),
    );
    expect(s.bucket).toBe("idle");
    expect(s.reason).toBe("abandoned agent session");
  });

  test("7. dirty takes precedence over the abandoned reason text (dirty wins the label)", () => {
    const s = deriveTaskState(
      signals({ sessionState: "abandoned", dirty: true, turnEndedAt: 100, lastFocusedAt: 200 }),
      manual(),
    );
    expect(s.bucket).toBe("idle");
    expect(s.reason).toBe("dirty");
  });
});

describe("deriveTaskState — precedence collisions", () => {
  test("asking + failing checks → needs-you/'agent is asking' (2a beats 2d)", () => {
    const s = deriveTaskState(
      signals({ sessionState: "asking", pr: pr({ checks: "failing" }) }),
      manual(),
    );
    expect(s.bucket).toBe("needs-you");
    expect(s.reason).toBe("agent is asking");
  });

  test("merged + asking → done (rung 1 beats rung 2 entirely)", () => {
    const s = deriveTaskState(
      signals({ mergedOrGone: true, sessionState: "asking" }),
      manual(),
    );
    expect(s.bucket).toBe("done");
    expect(s.reason).toBe("merged");
  });

  test("busyLock + unreviewed output → working (rung 3 beats rung 4)", () => {
    const s = deriveTaskState(
      signals({ busyLock: true, turnEndedAt: 100, lastFocusedAt: null }),
      manual(),
    );
    expect(s.bucket).toBe("working");
    expect(s.reason).toBe("worktree busy");
  });

  test("unreviewed output + approved/green PR → review-output (rung 4 beats rung 5)", () => {
    const s = deriveTaskState(
      signals({
        turnEndedAt: 100,
        lastFocusedAt: null,
        pr: pr({ reviewDecision: "APPROVED", checks: "passing" }),
      }),
      manual(),
    );
    expect(s.bucket).toBe("review-output");
  });

  test("approved/green PR + pending merge queue membership → ready (rung 5 beats rung 6)", () => {
    const s = deriveTaskState(
      signals({
        pr: pr({ reviewDecision: "APPROVED", checks: "passing", inMergeQueue: true }),
      }),
      manual(),
    );
    expect(s.bucket).toBe("ready");
  });

  test("conflict + midRebase → needs-you/'conflict mid-rebase' (2b beats 2c)", () => {
    const s = deriveTaskState(signals({ midRebase: true, conflict: true }), manual());
    expect(s.reason).toBe("conflict mid-rebase");
  });
});

describe("snooze semantics", () => {
  test("live snooze: snoozedBucket matches the computed bucket", () => {
    const s = deriveTaskState(signals({ dirty: true }), manual({ snoozedBucket: "idle" }));
    expect(s.bucket).toBe("idle");
    expect(s.snoozed).toBe(true);
  });

  test("stale snooze: snoozedBucket no longer matches → no effect on snoozed flag", () => {
    // Task was snoozed while "waiting", but is now needs-you — the
    // snooze target went stale and should not read as live.
    const s = deriveTaskState(
      signals({ sessionState: "asking" }),
      manual({ snoozedBucket: "waiting" }),
    );
    expect(s.bucket).toBe("needs-you");
    expect(s.snoozed).toBe(false);
  });

  test("snoozing never changes the bucket field itself", () => {
    const unsnoozed = deriveTaskState(signals({ dirty: true }), manual());
    const snoozed = deriveTaskState(signals({ dirty: true }), manual({ snoozedBucket: "idle" }));
    expect(snoozed.bucket).toBe(unsnoozed.bucket);
  });
});

describe("TASK_BUCKET_ORDER", () => {
  test("is exactly the documented order", () => {
    const expected: TaskBucket[] = [
      "needs-you",
      "review-output",
      "ready",
      "working",
      "waiting",
      "idle",
      "done",
    ];
    expect(TASK_BUCKET_ORDER).toEqual(expected);
  });
});

describe("compareTasks", () => {
  function key(bucket: TaskBucket, opts: { pinned?: boolean; snoozedBucket?: TaskBucket | null; lastActivityMs?: number; reason?: string } = {}) {
    const state = { bucket, reason: opts.reason ?? "x", snoozed: opts.snoozedBucket === bucket };
    return {
      state,
      manual: { pinned: opts.pinned ?? false, snoozedBucket: opts.snoozedBucket ?? null },
      lastActivityMs: opts.lastActivityMs ?? 0,
    };
  }

  test("pinned always sorts first, regardless of bucket", () => {
    const pinnedDone = key("done", { pinned: true });
    const unpinnedNeedsYou = key("needs-you");
    expect(compareTasks(pinnedDone, unpinnedNeedsYou)).toBeLessThan(0);
    expect(compareTasks(unpinnedNeedsYou, pinnedDone)).toBeGreaterThan(0);
  });

  test("bucket order otherwise governs: needs-you before waiting", () => {
    const a = key("needs-you");
    const b = key("waiting");
    expect(compareTasks(a, b)).toBeLessThan(0);
  });

  test("within a bucket, higher lastActivityMs (more recent) sorts first", () => {
    const newer = key("working", { lastActivityMs: 200 });
    const older = key("working", { lastActivityMs: 100 });
    expect(compareTasks(newer, older)).toBeLessThan(0);
  });

  test("a live-snoozed task ranks after idle but before done", () => {
    const snoozedNeedsYou = key("needs-you", { snoozedBucket: "needs-you" });
    const idle = key("idle");
    const done = key("done");
    expect(compareTasks(idle, snoozedNeedsYou)).toBeLessThan(0);
    expect(compareTasks(snoozedNeedsYou, done)).toBeLessThan(0);
  });

  test("a stale snooze does not demote — task sorts at its real bucket", () => {
    // snoozedBucket is "waiting" but the task's current bucket is
    // needs-you, so the snooze is stale: state.snoozed is false and
    // it should sort with the other needs-you tasks, ahead of idle.
    const staleSnoozed = key("needs-you", { snoozedBucket: "waiting" });
    const idle = key("idle");
    expect(compareTasks(staleSnoozed, idle)).toBeLessThan(0);
  });

  test("pinned beats live-snooze demotion too", () => {
    const pinnedButSnoozed = key("needs-you", { pinned: true, snoozedBucket: "needs-you" });
    const idle = key("idle");
    expect(compareTasks(pinnedButSnoozed, idle)).toBeLessThan(0);
  });

  test("full sort: pinned, then bucket order with snooze demotion, then recency", () => {
    const items = [
      key("idle", { lastActivityMs: 1 }),
      key("done", { pinned: true, lastActivityMs: 2 }),
      key("needs-you", { snoozedBucket: "needs-you", lastActivityMs: 3 }),
      key("needs-you", { lastActivityMs: 4 }),
      key("waiting", { lastActivityMs: 5 }),
      key("working", { lastActivityMs: 10 }),
      key("working", { lastActivityMs: 20 }),
    ];
    const sorted = [...items].sort(compareTasks);
    const shape = sorted.map((k) => `${k.manual.pinned ? "pin:" : ""}${k.state.bucket}${k.state.snoozed ? "(snoozed)" : ""}@${k.lastActivityMs}`);
    expect(shape).toEqual([
      "pin:done@2",
      "needs-you@4",
      "working@20",
      "working@10",
      "waiting@5",
      "idle@1",
      "needs-you(snoozed)@3",
    ]);
  });
});
