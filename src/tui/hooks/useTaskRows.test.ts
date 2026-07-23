import { describe, expect, test } from "bun:test";

import type { SessionTail } from "../../core/harness/claude/jsonl.ts";
import type { TaskBucket } from "../../core/task-state.ts";
import type { MergeQueueEntry, PullRequest } from "../../core/types.ts";
import {
  buildPrSignals,
  computeTurnEndedAt,
  mapReviewDecision,
  pickFocusMember,
  prChecksToTaskChecks,
  resolveTaskIndex,
  type TaskItem,
} from "./useTaskRows.ts";

/**
 * Pure-parts test suite for `useTaskRows`. Deliberately no React
 * rendering — everything exercised here is the plain-data half of the
 * hook (signal mapping, the turnEndedAt gate, the focus-slice picker,
 * the selection-index resolver), exported specifically so it's testable
 * without mounting the hook. The React-glued half (memoization, the
 * `useMemo` pipeline itself) isn't covered here — smoke-test that via
 * the TUI per the repo's testing conventions.
 */

describe("prChecksToTaskChecks", () => {
  test("maps every PrChecks value", () => {
    expect(prChecksToTaskChecks("pass")).toBe("passing");
    expect(prChecksToTaskChecks("fail")).toBe("failing");
    expect(prChecksToTaskChecks("pending")).toBe("pending");
    expect(prChecksToTaskChecks("none")).toBe("none");
  });
});

describe("mapReviewDecision", () => {
  test("maps every PrReview value", () => {
    expect(mapReviewDecision("approved")).toBe("APPROVED");
    expect(mapReviewDecision("changes_requested")).toBe("CHANGES_REQUESTED");
    expect(mapReviewDecision("pending")).toBe("REVIEW_REQUIRED");
    expect(mapReviewDecision("unrequested")).toBeNull();
    expect(mapReviewDecision("none")).toBeNull();
  });
});

describe("buildPrSignals", () => {
  function pr(overrides: Partial<PullRequest> = {}): PullRequest {
    return {
      number: 1,
      url: "https://github.com/x/y/pull/1",
      headRefName: "feature",
      baseRefName: "main",
      title: "A PR",
      isDraft: false,
      state: "OPEN",
      checks: "pass",
      failedChecks: [],
      review: "approved",
      reviewRequests: 0,
      requestedReviewers: [],
      suggestedReviewers: [],
      rabbit: { state: "none", unresolved: 0 },
      autoMerge: null,
      comments: [],
      unresolvedThreads: 0,
      ...overrides,
    };
  }

  test("null when the row has no PR at all", () => {
    expect(buildPrSignals(undefined, undefined, true)).toBeNull();
  });

  test("null when githubFresh is false, even with a PR present — persisted-cache PR data must never drive a bucket", () => {
    expect(buildPrSignals(pr(), undefined, false)).toBeNull();
  });

  test("maps fields through when a PR is present and github is fresh", () => {
    const mq: MergeQueueEntry = {
      headRefName: "feature",
      position: 1,
      state: "MERGEABLE",
      enqueuedAt: "2024-01-01T00:00:00Z",
      estimatedTimeToMerge: null,
    };
    const sig = buildPrSignals(
      pr({
        checks: "fail",
        review: "changes_requested",
        autoMerge: { enabledAt: "x", mergeMethod: "SQUASH" },
      }),
      mq,
      true,
    );
    expect(sig).toEqual({
      state: "OPEN",
      isDraft: false,
      checks: "failing",
      reviewDecision: "CHANGES_REQUESTED",
      autoMergeArmed: true,
      inMergeQueue: true,
    });
  });
});

describe("computeTurnEndedAt", () => {
  function tail(overrides: Partial<SessionTail> = {}): SessionTail {
    return {
      name: null,
      hasJsonl: true,
      lastEntryMs: 1000,
      lastEntryKind: "end_turn",
      queued: 0,
      pendingAsk: null,
      lastAssistantText: null,
      ...overrides,
    };
  }

  test("end_turn + waiting session → set (the freshest tail's terminal-ness agrees)", () => {
    expect(computeTurnEndedAt(tail({ lastEntryKind: "end_turn" }), "waiting")).toBe(1000);
  });

  test("end_turn + no live session (null) → set", () => {
    expect(computeTurnEndedAt(tail({ lastEntryKind: "end_turn" }), null)).toBe(1000);
  });

  test("paused → null — a mid-turn tool-permission stop is not a completed turn", () => {
    expect(computeTurnEndedAt(tail({ lastEntryKind: "paused" }), "waiting")).toBeNull();
    expect(computeTurnEndedAt(tail({ lastEntryKind: "paused" }), null)).toBeNull();
  });

  test("end_turn + working session → null — the session has resumed past the stale tail", () => {
    expect(computeTurnEndedAt(tail({ lastEntryKind: "end_turn" }), "working")).toBeNull();
  });

  test("end_turn + polling or asking session → null, same reasoning", () => {
    expect(computeTurnEndedAt(tail({ lastEntryKind: "end_turn" }), "polling")).toBeNull();
    expect(computeTurnEndedAt(tail({ lastEntryKind: "end_turn" }), "asking")).toBeNull();
  });

  test("null tail → null", () => {
    expect(computeTurnEndedAt(null, "waiting")).toBeNull();
  });

  test("tail with no lastEntryMs → null", () => {
    expect(computeTurnEndedAt(tail({ lastEntryMs: null }), "waiting")).toBeNull();
  });

  test("other terminal-looking kinds (tool_use, tool_result, other, null) never set turnEndedAt", () => {
    const nonTerminalKinds: SessionTail["lastEntryKind"][] = ["tool_use", "tool_result", "other", null];
    for (const kind of nonTerminalKinds) {
      expect(computeTurnEndedAt(tail({ lastEntryKind: kind }), null)).toBeNull();
    }
  });
});

describe("pickFocusMember", () => {
  type Member = { id: string; bucket: TaskBucket; ordinal: number };

  function pick(members: readonly Member[]): Member {
    return pickFocusMember(
      members,
      (m) => m.bucket,
      (m) => m.ordinal,
    );
  }

  test("highest-urgency member wins regardless of position", () => {
    const members: Member[] = [
      { id: "a", bucket: "idle", ordinal: 0 },
      { id: "b", bucket: "needs-you", ordinal: 1 },
      { id: "c", bucket: "waiting", ordinal: 2 },
    ];
    expect(pick(members).id).toBe("b");
  });

  test("ordinal tie-break: lower ordinal wins when buckets tie", () => {
    const members: Member[] = [
      { id: "a", bucket: "working", ordinal: 2 },
      { id: "b", bucket: "working", ordinal: 0 },
      { id: "c", bucket: "working", ordinal: 1 },
    ];
    expect(pick(members).id).toBe("b");
  });

  test("single member returns itself", () => {
    const members: Member[] = [{ id: "solo", bucket: "done", ordinal: 0 }];
    expect(pick(members).id).toBe("solo");
  });
});

describe("resolveTaskIndex", () => {
  // Minimal TaskItem stand-ins — only the fields resolveTaskIndex reads
  // (`kind`, `key`, `row.wt.slug`, `row.section`, `members[].wt.slug`)
  // need to be real; everything else is cast through `unknown` rather
  // than constructing a full `WorktreeRow` for a purely structural test.
  function wtItem(key: string, slug: string, section: string | null): TaskItem {
    return {
      kind: "wt",
      key,
      row: { wt: { slug }, section } as unknown,
      state: { bucket: "idle", reason: "idle", snoozed: false },
      manual: { pinned: false, snoozedBucket: null },
      detail: null,
      displayBucket: "idle",
      inExpandedStack: false,
    } as unknown as TaskItem;
  }

  function stackItem(key: string, memberSlugs: readonly string[]): TaskItem {
    const members = memberSlugs.map((slug) => ({ wt: { slug } }) as unknown);
    return {
      kind: "stack",
      key,
      row: members[0],
      state: { bucket: "idle", reason: "idle", snoozed: false },
      manual: { pinned: false, snoozedBucket: null },
      detail: null,
      displayBucket: "idle",
      label: key,
      members,
    } as unknown as TaskItem;
  }

  function prItem(url: string): TaskItem {
    return {
      kind: "pr",
      key: url,
      pr: { url } as unknown,
      state: { bucket: "needs-you", reason: "review requested", snoozed: false },
      detail: null,
      displayBucket: "needs-you",
    } as unknown as TaskItem;
  }

  test("sel === null → 0", () => {
    const tasks = [wtItem("a", "a", null), wtItem("b", "b", null)];
    expect(resolveTaskIndex(tasks, null)).toBe(0);
  });

  test("direct key match wins", () => {
    const tasks = [wtItem("a", "a", null), wtItem("b", "b", null)];
    expect(resolveTaskIndex(tasks, "b")).toBe(1);
  });

  test("slug matching an expanded stack member's row.section", () => {
    const tasks = [wtItem("x", "x", "stack-1"), wtItem("y", "y", "stack-1")];
    // "stack-1" isn't any item's own key or slug, but both members carry
    // it as their section — the first match wins.
    expect(resolveTaskIndex(tasks, "stack-1")).toBe(0);
  });

  test("slug matching a member of a collapsed stack", () => {
    const tasks = [wtItem("solo", "solo", null), stackItem("stack-1", ["m1", "m2"])];
    expect(resolveTaskIndex(tasks, "m2")).toBe(1);
  });

  test("no match falls back to 0", () => {
    const tasks = [wtItem("a", "a", null), wtItem("b", "b", null)];
    expect(resolveTaskIndex(tasks, "nonexistent")).toBe(0);
  });

  test("a pr-kind item never matches by slug", () => {
    const tasks = [prItem("https://github.com/x/y/pull/1"), wtItem("b", "b", null)];
    expect(resolveTaskIndex(tasks, "some-slug-not-present")).toBe(0);
  });
});
