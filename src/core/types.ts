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
 * Top-level body of the most recent submitted review that carried a
 * non-empty message. Distinct from `reviewThreads` — this is what the
 * reviewer types into the "Review changes" textarea on the Files tab,
 * not their inline line-anchored comments.
 */
export type LatestReview = {
  /** GitHub login of the reviewer. */
  author: string;
  /** The textarea body — may contain markdown and newlines. */
  body: string;
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
  /**
   * Most recent submitted review with a non-empty body. `null` when
   * nobody has left a top-level review message yet. Comment-only and
   * empty-body reviews are skipped at parse time.
   */
  latestReview: LatestReview | null;
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
