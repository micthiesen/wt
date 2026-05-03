/**
 * Typed query-key factory. Using `as const` tuples so each key is
 * narrowly typed — invalidation by prefix is safe (e.g. `qk.wt(slug)._all`
 * invalidates every query for that slug).
 */
export const qk = {
  /** All worktrees (from `git worktree list`). */
  worktrees: () => ["worktrees"] as const,
  /** Origin/main fetch marker; invalidated manually. */
  fetchOrigin: () => ["fetchOrigin"] as const,
  /** First-parent SHAs of origin/main; supports branchIsMerged. */
  mainFirstParents: () => ["mainFirstParents"] as const,
  /**
   * Combined GitHub fetch: PR map (one query per listed branch) +
   * merge-queue entries. Branches are sorted so the key is stable
   * regardless of worktree list order.
   */
  github: (branches: readonly string[]) =>
    ["github", [...branches].sort()] as const,
  /**
   * Repo-wide contributor list (sorted by commit count). Sits under
   * `["github"]` so it clears alongside the PR fetch on `refreshGithub`.
   */
  contributors: () => ["github", "contributors"] as const,
  /** Per-worktree property namespace. */
  wt: (slug: string) =>
    ({
      all: () => ["wt", slug] as const,
      dirty: () => ["wt", slug, "dirty"] as const,
      lock: () => ["wt", slug, "lock"] as const,
      deploy: () => ["wt", slug, "deploy"] as const,
      merged: () => ["wt", slug, "merged"] as const,
      gone: () => ["wt", slug, "gone"] as const,
      sync: () => ["wt", slug, "sync"] as const,
      claude: () => ["wt", slug, "claude"] as const,
      gitActivity: () => ["wt", slug, "gitActivity"] as const,
      firstCommit: () => ["wt", slug, "firstCommit"] as const,
      diffContext: () => ["wt", slug, "diffContext"] as const,
    }) as const,
  /**
   * AI summary keyed by worktree slug. Value carries the diff hash
   * inline (`{hash, title, brief, description}`); cross-diff sharing
   * lives in the memo below. Slug-stable so observers keep showing
   * the previous summary while a refetch is in flight after a diff
   * change — switching to a hash-based key would blank the brief
   * during the gap.
   */
  aiSummary: (slug: string) => ["aiSummary", slug] as const,
  /**
   * Content-addressed memo of LM Studio responses. Written by
   * `aiSummaryQuery` after a successful call; never observed directly.
   * Lets equivalent diffs across rebases / amends / branch renames
   * reuse the prior result without a new LM Studio round-trip.
   */
  aiSummaryMemo: (hash: string) => ["aiSummaryMemo", hash] as const,
  /** Manually-archived slug set (fs-backed). */
  archive: () => ["archive"] as const,
  /** Per-slug section + manual order (fs-backed). */
  wtState: () => ["wtState"] as const,
} as const;
