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
 * Review-bot state (CodeRabbit by default; configurable via
 * `[review_bot]`), derived from the bot's check contexts plus either
 * bot-authored review threads (`threads` mode) or the checkbox task
 * list in its latest summary comment (`checklist` mode). Unresolved
 * findings take precedence over a fresh "pending" — re-runs happen
 * routinely, but old feedback still needs addressing.
 */
export type ReviewBotStatus = {
  state: "pending" | "unresolved" | "clean" | "none";
  /** Count of unresolved bot findings. Only meaningful when state === "unresolved". */
  unresolved: number;
  /**
   * Checklist mode only: the bot has not reviewed THIS commit, so what
   * it did say describes an older diff. The findings are still
   * actionable; the flag says "reviewed an older commit" — and, since
   * green is the one badge color meaning "nothing to do here", it
   * downgrades a `clean` to the warning color rather than claiming a
   * commit nobody looked at came back empty.
   *
   * Answered by the bot's own check contexts where it has any on the
   * head (they hang off the head commit, so their presence IS the
   * answer), and only otherwise by the timestamp proxy of a summary
   * older than the head's `committedDate` — the shape of a reviewer
   * that deliberately never re-runs on push.
   */
  stale?: boolean;
};

/** Fallback for PR entries restored from a persisted cache that predates the field. */
export const REVIEW_BOT_NONE: ReviewBotStatus = { state: "none", unresolved: 0 };

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
 * GitHub's `PullRequest.mergeable`. `UNKNOWN` means GitHub hasn't
 * computed mergeability yet — it does so lazily, triggered by the
 * query itself — so consumers surface it as "computing" and re-ask
 * later rather than polling in a loop.
 */
export type MergeableState = "MERGEABLE" | "CONFLICTING" | "UNKNOWN";

/**
 * GitHub's `PullRequest.mergeStateStatus` — the merge-box verdict
 * (branch-protection blocks, behind-base, dirty = conflicts, ...).
 * Same lazy-`UNKNOWN` caveat as `MergeableState`.
 */
export type MergeStateStatus =
  | "BEHIND"
  | "BLOCKED"
  | "CLEAN"
  | "DIRTY"
  | "DRAFT"
  | "HAS_HOOKS"
  | "UNKNOWN"
  | "UNSTABLE";

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
  /**
   * GraphQL node ID, used by the arm-only auto-merge mutation.
   * Optional: entries hydrated from a persisted cache written before
   * the field existed lack it — callers must degrade gracefully (the
   * next live fetch fills it in).
   */
  id?: string;
  number: number;
  url: string;
  headRefName: string;
  /**
   * Head commit SHA at fetch time. The automations engine keys its
   * once-only fire ledger on this (a new push = a new failure instance).
   * Optional: entries restored from the persisted cache or an
   * older-daemon snapshot predate the field — the engine treats a
   * missing oid as "not fresh enough to evaluate".
   */
  headRefOid?: string;
  /**
   * The branch this PR targets. `config.branch.base` for trunk-targeted PRs;
   * another worktree's branch for stacked PRs. Used as a fallback signal for
   * `stackedOn` when commit-walk detection (the stronger signal) finds nothing.
   */
  baseRefName: string;
  /** GitHub's merged result commit; optional for older cached PR entries. */
  mergeCommitOid?: string | null;
  /** Human-authored PR title; preferred title source for the details pane. */
  title: string;
  isDraft: boolean;
  state: "OPEN" | "CLOSED" | "MERGED";
  /**
   * GitHub-computed mergeability (`UNKNOWN` while it's still being
   * computed — see `MergeableState`). Optional: entries restored from a
   * persisted cache written before the field existed lack it.
   */
  mergeable?: MergeableState | null;
  /** Merge-box verdict; same optionality/caveats as `mergeable`. */
  mergeStateStatus?: MergeStateStatus | null;
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
  /**
   * Review-bot status — its own track, separate from human reviews and
   * the CI rollup. `none` when the bot didn't run. Optional: entries
   * restored from the persisted cache predate the rename from `rabbit`;
   * read via `pr.reviewBot ?? REVIEW_BOT_NONE`.
   */
  reviewBot?: ReviewBotStatus;
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
   * Count of unresolved review threads opened by humans (bot threads
   * excluded). Surfaced as a "+N unresolved threads" summary line rather
   * than inlining every thread comment.
   */
  unresolvedThreads: number;
  /**
   * Count of unresolved review threads regardless of who opened them —
   * what GitHub's own PR page shows. Kept separate from
   * `unresolvedThreads` because the two answer different questions and
   * conflating them is actively misleading: in a repo where all review
   * is done by a bot, the human count is permanently 0, which reads as
   * "nothing outstanding" while the bot sits on unaddressed findings.
   */
  unresolvedThreadsTotal: number;
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
