export type Worktree = {
  path: string;
  branch: string;
  isMain: boolean;
  slug: string;
  stage: string;
};

export const StatusKind = {
  Busy: "busy",
  Missing: "missing",
  Gone: "gone",
  Merged: "merged",
  Dirty: "dirty",
  Clean: "clean",
} as const;
export type StatusKind = (typeof StatusKind)[keyof typeof StatusKind];

export type Status = {
  kind: StatusKind;
  label: string;
  age?: string;
  log?: string;
  pid?: number;
  op?: string;
};

export type PrChecks = "pass" | "fail" | "pending" | "none";

export type PrReview =
  | "approved"
  | "changes_requested"
  | "pending"
  | "unrequested"
  | "none";

/**
 * CodeRabbit review state, derived from the `CodeRabbit` status check
 * and CR-authored review threads. Unresolved threads take precedence
 * over a fresh "pending" — re-runs happen on every push, but old
 * feedback still needs addressing.
 */
export type RabbitStatus = {
  state: "pending" | "unresolved" | "clean" | "none";
  /** Count of unresolved CR-authored threads. Only meaningful when state === "unresolved". */
  unresolved: number;
};

export type SuggestedReviewer = {
  /** GitHub login (user). Teams aren't returned by `suggestedReviewers`. */
  login: string;
  isAuthor: boolean;
  isCommenter: boolean;
};

export type Contributor = {
  login: string;
  /** Total commits attributed to this user; the API sorts the list by this. */
  contributions: number;
};

/**
 * One entry in a PR's human conversation — either a plain issue comment
 * or the top-level body of a submitted review, flattened into a single
 * shape. Inline line-anchored review-thread comments are NOT represented
 * here (they're summarized as an unresolved-thread count instead).
 */
export type PrComment = {
  /** GitHub login of the author (bots already filtered out upstream). */
  author: string;
  /** The comment / review-textarea body; may contain markdown and newlines. */
  body: string;
  /** ISO timestamp. The list is sorted newest-first on this. */
  createdAt: string;
};

export type AutoMergeMethod = "SQUASH" | "MERGE" | "REBASE";

/**
 * "Merge when ready" state. Populated when someone has enabled
 * auto-merge on the PR and it's waiting on preconditions (CI, review,
 * base-behind). Clears automatically once the PR enters the merge
 * queue or merges. Mutually exclusive in practice with `MergeQueueEntry`.
 */
export type AutoMerge = {
  enabledAt: string;
  mergeMethod: AutoMergeMethod;
};

export type MergeQueueState =
  | "AWAITING_CHECKS"
  | "LOCKED"
  | "MERGEABLE"
  | "QUEUED"
  | "UNMERGEABLE";

export type MergeQueueEntry = {
  headRefName: string;
  position: number;
  state: MergeQueueState;
  enqueuedAt: string;
  estimatedTimeToMerge: number | null;
};

export type PullRequest = {
  number: number;
  url: string;
  headRefName: string;
  /**
   * The branch this PR targets. `config.branch.base` for trunk-targeted PRs;
   * another worktree's branch for stacked PRs. Used as a fallback signal for
   * `stackedOn` when commit-walk detection (the stronger signal) finds nothing.
   */
  baseRefName: string;
  /** Human-authored PR title; preferred title source for the details pane. */
  title: string;
  isDraft: boolean;
  state: "OPEN" | "CLOSED" | "MERGED";
  checks: PrChecks;
  /**
   * Names of the checks currently failing on this PR. Empty unless
   * `checks === "fail"`. Drives the details-pane failing-check line and
   * the `--log-failed` log tail; a `StatusContext` failure with no
   * associated Actions run still appears here even though it has no log.
   */
  failedChecks: readonly string[];
  /** Aggregated review state. `none` for terminal PRs (merged/closed). */
  review: PrReview;
  /** Outstanding review requests (humans + bots). */
  reviewRequests: number;
  /** Logins (users) and `org/team` slugs currently requested for review. */
  requestedReviewers: readonly string[];
  /** GitHub-suggested reviewers based on file ownership and history. */
  suggestedReviewers: readonly SuggestedReviewer[];
  /** CodeRabbit status — its own track, separate from human reviews. `none` when CR didn't run. */
  rabbit: RabbitStatus;
  /** "Merge when ready" arming state. `null` when not enabled. */
  autoMerge: AutoMerge | null;
  /**
   * The PR's human conversation: issue comments + non-empty review
   * bodies, merged and sorted newest-first, bots (CodeRabbit et al.)
   * excluded, capped at the most recent few. Empty when nobody human has
   * commented. Inline review-thread comments are not included here.
   */
  comments: readonly PrComment[];
  /**
   * Count of unresolved review threads opened by humans (CR / bot threads
   * excluded). Surfaced as a "+N unresolved threads" summary line rather
   * than inlining every thread comment.
   */
  unresolvedThreads: number;
  // ISO timestamps. Terminal PRs carry at least one of these; OPEN
  // PRs have neither. Used to dismiss pre-existing merged/closed PRs
  // when a worktree for the same branch is recreated from scratch.
  mergedAt?: string | null;
  closedAt?: string | null;
};

export type CheckStatus = "ok" | "warn" | "err" | "info";

export type Check = {
  name: string;
  status: CheckStatus;
  message: string;
  detail: string[];
};

export type SstStage = {
  name: string;
  sizeBytes: number;
  lastModified: string;
};

export type LockMeta = {
  op: string;
  phase: string;
  pid: number;
  host: string;
  startedAt: string; // ISO
  phase_started: string; // ISO
  // Legacy Python-era field; still read for back-compat.
  started?: string;
};
