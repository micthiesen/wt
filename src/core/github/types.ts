import type {
  AutoMergeMethod,
  MergeableState,
  MergeQueueEntry,
  MergeQueueState,
  MergeStateStatus,
  PrChecks,
  PullRequest,
} from "../types.ts";

/** One raw status-check node off the GraphQL statusCheckRollup. */
export type RawCheck =
  | {
      __typename: "CheckRun";
      name?: string | null;
      status?: string | null;
      conclusion?: string | null;
      /** Ordering key for superseded-run dedupe; absent on cached pre-v19 entries. */
      startedAt?: string | null;
      /**
       * Which Actions run produced this entry. `workflow.databaseId`
       * identifies the workflow FILE (stable across a rename, and distinct
       * for two files that happen to share a `name:`); `workflowRun
       * .databaseId` orders that file's runs. Absent on a check run from a
       * GitHub App rather than Actions, and on cached pre-v25 entries.
       */
      checkSuite?: {
        workflowRun?: {
          databaseId?: number | null;
          workflow?: { databaseId?: number | null } | null;
        } | null;
      } | null;
    }
  | {
      __typename: "StatusContext";
      context?: string | null;
      state?: string | null;
      /** Ordering key for superseded-run dedupe; absent on cached pre-v19 entries. */
      createdAt?: string | null;
    };

export type GqlReviewDecision = "APPROVED" | "CHANGES_REQUESTED" | "REVIEW_REQUIRED" | null;

export type GqlPrNode = {
  id: string;
  number: number;
  url: string;
  title: string;
  headRefName: string;
  headRefOid: string | null;
  baseRefName: string;
  mergeCommit?: { oid: string } | null;
  isDraft: boolean;
  state: PullRequest["state"];
  mergeable: MergeableState | null;
  mergeStateStatus: MergeStateStatus | null;
  mergedAt: string | null;
  closedAt: string | null;
  reviewDecision: GqlReviewDecision;
  reviewRequests: {
    totalCount: number;
    nodes: Array<{
      requestedReviewer:
        | { __typename: "User"; login: string }
        | { __typename: "Team"; combinedSlug: string }
        | null;
    }>;
  } | null;
  suggestedReviewers: Array<{
    reviewer: { login: string } | null;
    isAuthor: boolean;
    isCommenter: boolean;
  }> | null;
  autoMergeRequest: { enabledAt: string; mergeMethod: AutoMergeMethod } | null;
  commits: {
    nodes: Array<{
      commit: {
        committedDate?: string | null;
        statusCheckRollup: { contexts: { nodes: RawCheck[] } } | null;
      };
    }>;
  };
  reviewThreads: { nodes: GqlReviewThread[] } | null;
  comments: {
    nodes: Array<{
      author: GqlCommentAuthor;
      body: string;
      createdAt: string;
      updatedAt?: string | null;
    }>;
  } | null;
  reviews: {
    nodes: Array<{
      author: GqlCommentAuthor;
      body: string;
      state: GqlReviewSubmissionState;
      createdAt: string;
    }>;
  } | null;
};

export type GqlReviewSubmissionState =
  | "APPROVED"
  | "CHANGES_REQUESTED"
  | "COMMENTED"
  | "DISMISSED"
  | "PENDING";

export type GqlCommentAuthor = { login: string | null; __typename?: string } | null;

export type GqlReviewThread = {
  isResolved: boolean;
  comments: { nodes: Array<{ author: GqlCommentAuthor }> };
};

export type GqlMqEntry = {
  enqueuedAt: string;
  estimatedTimeToMerge: number | null;
  position: number;
  state: MergeQueueState;
  pullRequest: { headRefName: string } | null;
};

export type GqlRepo = {
  mergeQueue?: { entries?: { nodes?: GqlMqEntry[] } } | null;
  // Each aliased `wt_N` key lands here as `{ nodes: GqlPrNode[] }`.
  [alias: `wt_${number}`]: { nodes?: GqlPrNode[] } | undefined;
};

export type GqlResponse = {
  data?: { repository?: GqlRepo };
  errors?: Array<{ message?: string; type?: string }>;
};

export type GithubData = {
  prs: Map<string, PullRequest>;
  mergeQueue: Map<string, MergeQueueEntry>;
};

/**
 * A pull request the authenticated user has been asked to review. Not a
 * worktree (we typically don't have a local checkout of someone else's
 * branch), just a pinned list at the bottom of the TUI. Carries the
 * minimum surface needed for the list label, the lite details pane, and
 * the `p` open-in-browser action.
 */
export type ReviewRequestPr = {
  number: number;
  url: string;
  title: string;
  repoNameWithOwner: string;
  headRefName: string | null;
  author: string | null;
  isDraft: boolean;
  checks: PrChecks;
  /** GitHub's aggregate review state, or null when none recorded yet. */
  reviewDecision: "APPROVED" | "CHANGES_REQUESTED" | "REVIEW_REQUIRED" | null;
  additions: number;
  deletions: number;
  changedFiles: number;
  commentCount: number;
  createdAt: string;
  updatedAt: string;
};

/**
 * `retryable` marks a failure whose CAUSE is transient and clears on
 * its own, so a caller with a retry budget (the automation engine)
 * can tell "this will work shortly" from "this will never work".
 * Absent means unknown, which callers must read as NOT retryable.
 */
export type GhActionResult =
  | { ok: true }
  | { ok: false; error: string; retryable?: boolean };

export type LivePrInfo = {
  number: number;
  baseRefName: string;
  state: "OPEN" | "CLOSED" | "MERGED";
  isDraft: boolean;
  title: string;
  /**
   * GraphQL node id and head sha — what the merge mutations need, and
   * neither is derivable from the number. `enqueuePullRequest` takes
   * the head as `expectedHeadOid` so a push between the read and the
   * mutation refuses instead of queueing a commit nobody saw. Empty
   * when gh returned neither (an older gh, a malformed payload).
   */
  id: string;
  headRefOid: string;
};
