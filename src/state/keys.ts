/**
 * Typed query-key factory. Using `as const` tuples so each key is
 * narrowly typed — invalidation by prefix is safe (e.g.
 * `qc.invalidateQueries({ queryKey: qk.wt(slug).all() })` invalidates
 * every query for that slug).
 */
export const qk = {
  /** All worktrees (from `git worktree list`). */
  worktrees: () => ["worktrees"] as const,
  issueStatuses: (ids: readonly string[], command: readonly string[] | null, cwd: string) =>
    ["issueStatuses", command, cwd, [...ids].sort()] as const,
  /** Worktrees owned by one SSH host; no arg is the all-hosts prefix. */
  remoteWorktrees: (host?: string) =>
    host ? (["remoteWorktrees", host] as const) : (["remoteWorktrees"] as const),
  remoteWorkerInfo: (host?: string) =>
    host ? (["remoteWorkerInfo", host] as const) : (["remoteWorkerInfo"] as const),
  /**
   * Set of slug names with a live interactive tmux session on the
   * wt-private server. One CLI call per refresh regardless of worktree
   * count — drives the per-row session indicator and the details-pane
   * "session attached" hint.
   */
  tmuxSessions: () => ["tmuxSessions"] as const,
  /**
   * Harness session discovery for one (slug, harness) pair. Each impl
   * (`core/harness/<id>.ts`) defines what "discoverable" means: Claude
   * reads its persisted-name file + jsonl tails + the registry; Codex
   * scans rollouts under `~/.codex/sessions/`; OpenCode reads
   * `opencode.db`. Live-status is annotated on top by the consumer
   * hook against `tmuxSessionsQuery.all`, so the key intentionally
   * does NOT include the tmux name set — that would invalidate this
   * query on every tmux-sessions refresh.
   */
  harnessSessions: (harnessId: string, slug: string) =>
    ["harnessSessions", harnessId, slug] as const,
  /**
   * Currently-selected primary harness id. Persisted at
   * `<cacheRoot>/harness.json`, falling back to `[harness].primary`. Drives F12 / top-right indicator /
   * TAB cycle. Single global key.
   */
  primaryHarness: () => ["primaryHarness"] as const,
  /** Origin/main fetch marker; invalidated manually. */
  fetchOrigin: () => ["fetchOrigin"] as const,
  /**
   * GitHub PR fetch: one aliased query per listed branch. Branches are
   * sorted so the key is stable regardless of worktree list order.
   */
  github: (branches: readonly string[]) =>
    ["github", [...branches].sort()] as const,
  /**
   * Tips of the branches a `branch.advanced` automation watches. Keyed
   * on the sorted branch list, like `github` — a config edit that adds
   * or drops a watched branch produces a new key rather than a stale
   * entry answering about a set nobody asked for.
   */
  watchedBranchTips: (branches: readonly string[]) =>
    ["watchedBranchTips", [...branches].sort()] as const,
  productionCommits: (branch: string, tip: string, commits: readonly string[]) =>
    ["productionCommits", branch, tip, [...commits].sort()] as const,
  /**
   * Pull requests where the authenticated user has been requested as
   * reviewer. Single global key — the GraphQL `search` doesn't take a
   * worktree-keyed parameter. Intentionally NOT under the `["github"]`
   * prefix: the per-branch PR cache and this list have different
   * `data` shapes, and `mutate({ filter: { queryKey: ["github"] } })`
   * iterates every matching entry via `setQueriesData`. A shared
   * prefix would push `ReviewRequestPr[]` through `patchPullRequest`,
   * which expects `GithubData.prs[branch]` and throws on the wrong
   * shape. `refreshAll` / `refreshGithub` invalidate this key
   * explicitly to keep refresh semantics.
   */
  reviewRequests: () => ["reviewRequests"] as const,
  /**
   * Repo-wide contributor list (sorted by commit count). Lives outside
   * the `["github"]` prefix on purpose — `refreshGithub`/`refreshAll`
   * blow that prefix away every time the user hits `r`, but the
   * contributor set drifts on a scale of weeks, not minutes. Letting it
   * survive cross-refresh is the difference between "reviewer picker
   * opens instantly" and "reviewer picker pays a 6-round-trip refetch
   * after every refresh".
   */
  contributors: () => ["contributors"] as const,
  /**
   * Anthropic API utilization snapshot read from the Claude Code
   * statusline cache. Single global key — the cache is per-user, not
   * per-worktree.
   */
  claudeUsage: () => ["claudeUsage"] as const,
  /** Codex rate-limit usage (5h/7d %), read from the newest rollout. */
  codexUsage: () => ["codexUsage"] as const,
  /** OpenCode spend (5h/7d $), summed from its message-cost rows. */
  opencodeCost: () => ["opencodeCost"] as const,
  /**
   * Live registry of running claude processes from
   * `~/.claude/sessions/<pid>.json`. Single global key; consumers
   * filter by sessionId or cwd. Invalidated by fs.watch in the TUI
   * runtime on every file event — polling is the slow backstop.
   */
  claudeRegistry: () => ["claudeRegistry"] as const,
  /**
   * Per-worktree LLM-authored session summaries — `ai-title` /
   * `away_summary` / `last-prompt` snippets pulled from each
   * session's jsonl. Single key per slug; the queryFn derives the
   * sessionId set internally from the persisted name list, so name
   * churn doesn't require a key change (the cache stays warm and the
   * (mtime, size) memo inside the reader covers correctness).
   */
  claudeSummaries: (slug: string) => ["claudeSummaries", slug] as const,
  /**
   * Live process/CPU snapshot behind the `P` overlay. Single global
   * key; sampled only while that overlay is open and never persisted
   * (a restored snapshot is a previous run's dead pids).
   */
  perf: () => ["perf"] as const,
  /** Per-worktree property namespace. */
  wt: (slug: string) =>
    ({
      all: () => ["wt", slug] as const,
      dirty: () => ["wt", slug, "dirty"] as const,
      lock: () => ["wt", slug, "lock"] as const,
      deploy: () => ["wt", slug, "deploy"] as const,
      /**
       * `[dev_server]` supervisor state (session + port probe). Keyed
       * by the batched tmux session-existence read (`null` while that
       * query hasn't loaded yet) so a flip immediately cache-misses
       * into a fresh probe instead of waiting out the staleTime — the
       * same "value drives the key" pattern as `sync`/`gitActivity`.
       */
      dev: (sessionExists: boolean | null) =>
        ["wt", slug, "dev", sessionExists] as const,
      /**
       * "Did this BRANCH land" / "is its remote ref gone" — both are
       * facts about the branch, so the branch drives the key, same as
       * `sync`/`gitActivity` above. Keyed on the slug alone they
       * outlived the branch they described: repoint a worktree at a
       * new branch and the answer computed for the OLD one is served
       * for the new, persisted across restarts like every other query.
       *
       * That is not a display nit. `wt.merged` automations read these
       * through `row.status`, so a row whose previous branch had
       * merged fired `builtin:delete-branch` against its NEW branch —
       * deleting the remote ref of an open PR, which GitHub then
       * closed as a side effect of the ref vanishing. Observed on
       * coz-2100-open-internet: #1618 merged one day, #1631 opened
       * three minutes before the second firing and was never merged
       * at all.
       */
      merged: (branch: string) => ["wt", slug, "merged", branch] as const,
      gone: (branch: string) => ["wt", slug, "gone", branch] as const,
      /**
       * Sync counts vs (a) `origin/<branch>` and (b) effective base. Keyed by
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
      firstCommit: (base: string | null = null) =>
        ["wt", slug, "firstCommit", base] as const,
      /**
       * Diff context keyed by both slug and effective base. When the
       * base flips (e.g. stack parent changes from `origin/main` to a
       * sibling branch) the cache lookup misses, the query refetches
       * with the new base, and the AI summary picks up the new hash.
       */
      diffContext: (base: string) =>
        ["wt", slug, "diffContext", base] as const,
      /**
       * Rebase-conflict pre-flight vs the effective base (a `git
       * merge-tree` dry-run). Base in the key for the same reason as
       * `sync` / `gitActivity` / `diffContext`: the answer depends on
       * which base we'd rebase onto.
       */
      conflict: (base: string) => ["wt", slug, "conflict", base] as const,
      /**
       * Prefix over every base's conflict entry — for push
       * invalidation from the rebase-state watcher, which knows the
       * slug but not which base the observer is currently keyed on.
       */
      conflictAny: () => ["wt", slug, "conflict"] as const,
    }) as const,
  /**
   * AI summary keyed by content hash of the diff. Equivalent diffs
   * across rebases / amends / branch renames share a single cache
   * entry. The "show the previous summary while a new hash fetches"
   * behavior comes from `placeholderData: keepPreviousData` at the
   * observer rather than a separate slug-stable layer.
   */
  aiSummary: (hash: string) => ["aiSummary", hash] as const,
  /**
   * AI-named stack section title, keyed by a signature over the
   * sorted member branch names. Stays warm across commit churn
   * within a fixed member set; only membership changes cut new
   * cache entries. Persisted alongside other AI summaries.
   */
  stackTitle: (sig: string) => ["stackTitle", sig] as const,
  /** Manually-archived slug set (fs-backed). */
  archive: () => ["archive"] as const,
  /**
   * Per-slug section + manual order + fork-base records (fs-backed).
   * The `baseBranch` records here are the single source for stack
   * relationships (membership, order, diff base) — `refreshStack`
   * invalidates this key.
   */
  wtState: () => ["wtState"] as const,
} as const;
