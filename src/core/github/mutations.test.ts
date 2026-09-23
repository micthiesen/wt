/**
 * The merge-queue refusal classifier. Every `enqueuePullRequest`
 * failure comes back as GraphQL type `UNPROCESSABLE` with prose, so
 * "this PR isn't ready YET" and "this call is wrong" are the same error
 * code and only the message separates them — which is why this has to
 * fail closed. A false positive arms a DIFFERENT GitHub feature than
 * the one the keystroke asked for.
 */
import { describe, expect, test } from "bun:test";

import { checksStillPending, DEQUEUE_PULL_REQUEST_MUTATION, mergeArmKind, missingWorkflowScope, notYetEnqueueable, PR_MERGE_ARM_QUERY } from "./mutations.ts";

test("dequeue uses DequeuePullRequestInput.id, not enqueue's pullRequestId", () => {
  expect(DEQUEUE_PULL_REQUEST_MUTATION).toContain("dequeuePullRequest(input: {id: $prId})");
  expect(DEQUEUE_PULL_REQUEST_MUTATION).not.toContain("pullRequestId:");
});

test("cancel probes the PR's actual queue entry and classic arm", () => {
  expect(PR_MERGE_ARM_QUERY).toContain("mergeQueueEntry { id }");
  expect(PR_MERGE_ARM_QUERY).toContain("autoMergeRequest { enabledAt }");
  // #2146: staging has a queue, but classic auto-merge was armed while
  // the PR was not queued. Branch configuration cannot decide cancellation.
  expect(mergeArmKind({ mergeQueueEntry: null, autoMergeRequest: { enabledAt: "2026-09-23T19:52:58Z" } })).toBe("classic");
  expect(mergeArmKind({ mergeQueueEntry: { id: "MQE_1" }, autoMergeRequest: null })).toBe("queue");
  expect(mergeArmKind({ mergeQueueEntry: { id: "MQE_1" }, autoMergeRequest: { enabledAt: "now" } })).toBe("both");
  expect(mergeArmKind({ mergeQueueEntry: null, autoMergeRequest: null })).toBe("none");
  expect(mergeArmKind(null)).toBe("none");
});

describe("notYetEnqueueable", () => {
  // Observed verbatim on a live queue base while a required check had
  // not yet reported (wt app log, 2026-08-21).
  test("a queue refusing an unready PR is retryable as classic arming", () => {
    expect(
      notYetEnqueueable(
        "gh: Pull request 2 of 4 required status checks have not succeeded: 1 expected.",
      ),
    ).toBe(true);
  });

  test("other unprocessable refusals are not", () => {
    // Also observed live: a stale cached head oid. Arming classic
    // auto-merge here would merge a commit the user never saw.
    expect(
      notYetEnqueueable(
        "gh: Failed to add PR #1411: expected head oid does not match the current head oid",
      ),
    ).toBe(false);
    expect(notYetEnqueueable("gh: Could not resolve to a node with the global id of ''")).toBe(
      false,
    );
    expect(notYetEnqueueable("gh: Pull request is already queued")).toBe(false);
  });

  // Absence of a value means unknown, never "fine".
  test("a missing or empty error is not a retry signal", () => {
    expect(notYetEnqueueable(undefined)).toBe(false);
    expect(notYetEnqueueable("")).toBe(false);
  });
});

describe("checksStillPending", () => {
  // Verbatim from the refusal that produced this: wt armed at
  // 23:55:21Z and the workflow created "Unit tests (Vitest)" at
  // 23:56:23Z, 62 seconds later.
  const REAL = 'gh: Pull request Required status check "Unit tests (Vitest)" is expected.';
  // Verbatim from PR #1424, the SAME check name minutes later in its
  // life. This one refuted the belief that a queue takes a PR whose
  // required checks are merely running.
  const RUNNING =
    'gh: Pull request Required status check "Unit tests (Vitest)" is in progress.';

  test("recognises a required check that has not reported at all", () => {
    expect(checksStillPending(REAL)).toBe(true);
  });

  test("recognises a required check that is still running", () => {
    expect(checksStillPending(RUNNING)).toBe(true);
  });

  // Verbatim from PR #1446. The aggregate wording was read as ambiguous
  // and failed closed as a class, on the reasoning that it "mixes
  // pending with failed". It does not mix anything here: GitHub
  // enumerates the reasons and both of these name only checks that have
  // yet to report.
  const AGGREGATE = "gh: Pull request 4 of 4 required status checks have not succeeded: 2 expected.";

  test("reads the aggregate breakdown instead of refusing the whole class", () => {
    expect(checksStillPending(AGGREGATE)).toBe(true);
    expect(
      checksStillPending(
        "Pull request 2 of 4 required status checks have not succeeded: 1 expected.",
      ),
    ).toBe(true);
  });

  test("a breakdown listing several pending states is still a clock", () => {
    expect(
      checksStillPending(
        "Pull request 3 of 4 required status checks have not succeeded: 1 expected, 1 pending, and 1 in progress.",
      ),
    ).toBe(true);
  });

  test("one failing entry disqualifies the whole breakdown", () => {
    // The reason the class was refused wholesale, now handled one level
    // down: waiting does not clear a failure, and a retry loop would put
    // a spinner in front of it.
    for (const reasons of ["1 failing", "1 expected and 1 failing", "2 cancelled"]) {
      expect(
        checksStillPending(`Pull request 2 of 4 required status checks have not succeeded: ${reasons}.`),
      ).toBe(false);
    }
  });

  test("an unrecognised or absent breakdown is not a green light", () => {
    // Absence of a reason means unknown, never fine — and a word GitHub
    // adds next year must not arrive already classified as harmless.
    expect(
      checksStillPending("Pull request 2 of 4 required status checks have not succeeded: 1 blorped."),
    ).toBe(false);
    expect(
      checksStillPending("Pull request 2 of 4 required status checks have not succeeded."),
    ).toBe(false);
  });

  test("a check that FAILED is never retryable — someone has to re-run it", () => {
    expect(
      checksStillPending('Pull request Required status check "Unit tests (Vitest)" has failed.'),
    ).toBe(false);
  });

  test("does not fire on the wrong-commit refusal", () => {
    expect(
      checksStillPending("expected head oid does not match the current head oid"),
    ).toBe(false);
  });

  test("absent and unrecognised messages are not retryable", () => {
    expect(checksStillPending(undefined)).toBe(false);
    expect(checksStillPending("Auto merge is not allowed for this repository")).toBe(false);
  });

  test("pending is a subset of not-yet-enqueueable, so ordering is what separates them", () => {
    // Both match the real messages; `enableAutoMerge` therefore has to
    // test the narrower one FIRST or the retryable flag is never set.
    for (const msg of [REAL, RUNNING, AGGREGATE]) {
      expect(notYetEnqueueable(msg)).toBe(true);
      expect(checksStillPending(msg)).toBe(true);
    }
  });
});

/**
 * The verbatim refusal from cozee-dev #1572, a `staging` PR touching
 * `.github/workflows/ci.yml` on a token whose scopes were
 * `admin:public_key, gist, read:org, repo`. It reads as a fact about the
 * pull request and is a fact about the local token, and it names no
 * remedy at all.
 */
describe("missingWorkflowScope", () => {
  test("the OAuth App wording, as GitHub sends it", () => {
    expect(
      missingWorkflowScope(
        "gh: Pull request refusing to allow an OAuth App to create or update workflow `.github/workflows/ci.yml` without `workflow` scope",
      ),
    ).toBe(true);
  });

  test("the other two credential kinds, which differ only in the noun", () => {
    expect(
      missingWorkflowScope(
        "refusing to allow a GitHub App to create or update workflow `.github/workflows/ci.yml` without `workflow` scope",
      ),
    ).toBe(true);
    expect(
      missingWorkflowScope(
        "refusing to allow a Personal Access Token to create or update workflow `.github/workflows/release.yml` without `workflow` scope",
      ),
    ).toBe(true);
  });

  test("either half alone is enough — the two clauses do not always travel together", () => {
    expect(missingWorkflowScope("refusing to allow an OAuth App to create or update workflow x.yml")).toBe(true);
    expect(missingWorkflowScope("resource not accessible without workflow scope")).toBe(true);
  });

  test("an unrelated refusal is not claimed", () => {
    // The queue's own refusals must keep their own handling: mislabelling
    // one as a scope problem sends the reader to re-auth over a wait.
    expect(missingWorkflowScope('gh: Required status check "Lint & Typecheck" is expected.')).toBe(false);
    expect(missingWorkflowScope("gh: Auto merge is not allowed for this repository")).toBe(false);
    expect(missingWorkflowScope("gh: Pull request is already queued")).toBe(false);
    expect(missingWorkflowScope(undefined)).toBe(false);
  });

  test("it is never retryable, so it must not look like the pending class", () => {
    const err =
      "gh: Pull request refusing to allow an OAuth App to create or update workflow `.github/workflows/ci.yml` without `workflow` scope";
    expect(checksStillPending(err)).toBe(false);
    expect(notYetEnqueueable(err)).toBe(false);
  });
});
