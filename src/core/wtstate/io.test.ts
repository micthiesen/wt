import { describe, expect, test } from "bun:test";

import { parseWtState } from "./io.ts";

/**
 * parseWtState is the tolerant-parse boundary for a hand-editable,
 * version-drifting file — these tests pin the field-by-field
 * degradation semantics (a bad field drops, the rest of the record
 * survives) via plain JSON round-trips, no fs involved.
 */
describe("parseWtState", () => {
  test("keeps removed outcome and tracker override, including an explicit unlink", () => {
    const state = parseWtState({ removed: [
      { slug: "a", branch: "m/a", removedAt: "2026-09-23T00:00:00Z", issueId: "COZ-1445", githubIssue: 2116, gitState: "merged" },
      { slug: "b", branch: "m/b", removedAt: "2026-09-23T00:00:00Z", issueId: "", githubIssue: -1, gitState: "gone" },
      { slug: "c", branch: "m/c", removedAt: "2026-09-23T00:00:00Z", gitState: "impossible" },
    ] });
    expect(state.removed.map(({ issueId, githubIssue, gitState }) => ({ issueId, githubIssue, gitState }))).toEqual([
      { issueId: "COZ-1445", githubIssue: 2116, gitState: "merged" },
      { issueId: "", githubIssue: undefined, gitState: "gone" },
      { issueId: undefined, githubIssue: undefined, gitState: undefined },
    ]);
  });
  test("accepts only proved release snapshot fields", () => {
    const state = parseWtState({ removed: [
      { slug: "a", branch: "m/a", removedAt: "2026-09-23T00:00:00Z", landedOnAtRemoval: "base", prMergeCommitOid: "abc" },
      { slug: "b", branch: "m/b", removedAt: "2026-09-23T00:00:00Z", landedOnAtRemoval: "maybe", prMergeCommitOid: "" },
    ] });
    expect(state.removed.map(({ landedOnAtRemoval, prMergeCommitOid }) =>
      ({ landedOnAtRemoval, prMergeCommitOid }))).toEqual([
      { landedOnAtRemoval: "base", prMergeCommitOid: "abc" },
      { landedOnAtRemoval: undefined, prMergeCommitOid: undefined },
    ]);
  });
  test("retains creation identity without inventing one for legacy or invalid records", () => {
    const state = parseWtState({ slugs: { fresh: { createdAt: "2026-09-22T12:00:00.000Z" }, old: {}, invalid: { createdAt: "oops" } } });
    expect(state.slugs.fresh?.createdAt).toBe("2026-09-22T12:00:00.000Z");
    expect(state.slugs.old?.createdAt).toBeUndefined();
    expect(state.slugs.invalid?.createdAt).toBeUndefined();
  });
  test("round-trips a full slug record including the work status", () => {
    const state = parseWtState({
      slugs: {
        "eng-1-foo": {
          section: "Now",
          order: 3,
          baseBranch: "michael/eng-0-base",
          baseSha: "abc123",
          githubIssue: 42,
          work: {
            state: "ready",
            note: "calendar integrations may need a resync",
            risk: "medium",
            at: "2026-08-08T12:00:00.000Z",
            sha: "def456",
          },
        },
      },
    });
    expect(state.slugs["eng-1-foo"]).toEqual({
      section: "Now",
      order: 3,
      baseBranch: "michael/eng-0-base",
      baseSha: "abc123",
      githubIssue: 42,
      work: {
        state: "ready",
        note: "calendar integrations may need a resync",
        risk: "medium",
        at: "2026-08-08T12:00:00.000Z",
        sha: "def456",
      },
    });
  });

  test("round-trips controller-owned remote layouts and discovers their sections", () => {
    const state = parseWtState({
      remoteLayouts: {
        "@remote/dellserver/remote-task": { section: "Remote batch", order: 4 },
      },
      sectionsOrder: ["\0inbox"],
    });
    expect(state.remoteLayouts).toEqual({
      "@remote/dellserver/remote-task": { section: "Remote batch", order: 4 },
    });
    expect(state.sectionsOrder).toContain("Remote batch");
  });

  test("round-trips valid review-request dismissals and drops malformed entries", () => {
    const state = parseWtState({
      reviewRequestDismissals: [
        {
          url: "https://github.com/example/repo/pull/12",
          updatedAt: "2026-09-09T10:00:00Z",
          dismissedAt: "2026-09-09T10:01:00Z",
        },
        { url: "https://github.com/example/repo/pull/13" },
        "bad",
      ],
    });
    expect(state.reviewRequestDismissals).toEqual([
      {
        url: "https://github.com/example/repo/pull/12",
        updatedAt: "2026-09-09T10:00:00Z",
        dismissedAt: "2026-09-09T10:01:00Z",
      },
    ]);
  });

  test("drops a malformed work record without dropping the slug", () => {
    const state = parseWtState({
      slugs: {
        a: { section: null, order: 0, work: { state: "shipped", at: "t" } },
        b: { section: null, order: 1, work: "ready" },
      },
    });
    expect(state.slugs.a).toEqual({ section: null, order: 0 });
    expect(state.slugs.b).toEqual({ section: null, order: 1 });
  });

  test("sanitizes control characters in a persisted note", () => {
    const state = parseWtState({
      slugs: {
        a: {
          section: null,
          order: 0,
          work: { state: "needs-human", note: "log[31m me in", at: "t" },
        },
      },
    });
    expect(state.slugs.a!.work?.note).toBe("log me in");
  });

  test("tolerates invalid githubIssue / devPort and legacy hub fields", () => {
    const state = parseWtState({
      slugs: {
        a: {
          section: "S",
          order: 1,
          githubIssue: -5,
          devPort: 99_999_999,
          // Removed hub-era fields must be silently dropped, not kept.
          taskPinned: true,
          taskSnoozedBucket: "needs-you",
        },
      },
    });
    expect(state.slugs.a).toEqual({ section: "S", order: 1 });
  });

  test("empty / garbage input degrades to an empty state", () => {
    for (const raw of [null, undefined, 42, "x", { slugs: "nope" }]) {
      const state = parseWtState(raw);
      expect(state.slugs).toEqual({});
      expect(state.remoteLayouts).toEqual({});
      expect(state.reviewRequestDismissals).toEqual([]);
      expect(Array.isArray(state.sectionsOrder)).toBe(true);
    }
  });
});
