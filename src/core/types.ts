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

export type PrReview = "approved" | "changes_requested" | "pending" | "none";

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
  /** Human-authored PR title; preferred title source for the details pane. */
  title: string;
  isDraft: boolean;
  state: "OPEN" | "CLOSED" | "MERGED";
  checks: PrChecks;
  /** Aggregated review state. `none` for terminal PRs (merged/closed). */
  review: PrReview;
  /** Outstanding review requests (humans + bots). */
  reviewRequests: number;
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
