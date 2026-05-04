/**
 * Typed query-key factory. Using `as const` tuples so each key is
 * narrowly typed — invalidation by prefix is safe (e.g.
 * `qc.invalidateQueries({ queryKey: qk.wt(slug).all() })` invalidates
 * every query for that slug).
 */
export const qk = {
  /** All worktrees (from `git worktree list`). */
  worktrees: () => ["worktrees"] as const,
  /**
   * Set of slug names with a live interactive tmux session on the
   * wt-private server. One CLI call per refresh regardless of worktree
   * count — drives the per-row session indicator and the details-pane
   * "session attached" hint.
   */
  tmuxSessions: () => ["tmuxSessions"] as const,
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
      /**
       * Sync counts vs (a) `@{u}` and (b) effective base. Keyed by
       * the resolved base so a stack-parent flip cache-misses into a
       * refetch — same pattern as `diffContext` and for the same
       * reason: the answer depends on which base we're comparing to.
       */
      sync: (base: string) => ["wt", slug, "sync", base] as const,
      claude: () => ["wt", slug, "claude"] as const,
      /**
       * Git activity (timestamps + shortstat). Includes the effective
       * base in the key for the same reason as `sync` and
       * `diffContext`: the diff line counts depend on the base, so
       * each base lives in its own cache slot.
       */
      gitActivity: (base: string) =>
        ["wt", slug, "gitActivity", base] as const,
      firstCommit: () => ["wt", slug, "firstCommit"] as const,
      /**
       * Diff context keyed by both slug and effective base. When the
       * base flips (e.g. stack parent changes from `origin/main` to a
       * sibling branch) the cache lookup misses, the query refetches
       * with the new base, and the AI summary picks up the new hash.
       */
      diffContext: (base: string) =>
        ["wt", slug, "diffContext", base] as const,
    }) as const,
  /**
   * AI summary keyed by content hash of the diff. Equivalent diffs
   * across rebases / amends / branch renames share a single cache
   * entry. The "show the previous summary while a new hash fetches"
   * behavior comes from `placeholderData: keepPreviousData` at the
   * observer rather than a separate slug-stable layer.
   */
  aiSummary: (hash: string) => ["aiSummary", hash] as const,
  /** Manually-archived slug set (fs-backed). */
  archive: () => ["archive"] as const,
  /** Per-slug section + manual order (fs-backed). */
  wtState: () => ["wtState"] as const,
  /**
   * Cross-worktree stack detection: for each slug, the slug+branch of
   * the parent worktree it's stacked on (commit-ancestry signal). Keyed
   * by the sorted branch list so adding/removing a worktree
   * re-triggers; SHA drift inside a fixed branch set is picked up via
   * the staleTime on the query rather than a key change.
   */
  stack: (branches: readonly string[]) =>
    ["stack", [...branches].sort()] as const,
} as const;
