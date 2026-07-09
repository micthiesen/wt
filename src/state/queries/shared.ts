import { keepPreviousData } from "@tanstack/react-query";

/**
 * Sentinel hash for `aiSummaryQuery` when the caller hasn't built a
 * diff context yet. The query is gated by `enabled: !!ctx` so the
 * queryFn never runs against this key — but the queryKey still has to
 * be a string. A named constant makes the intent obvious vs. a magic
 * `"__noctx__"` literal.
 */
export const NO_CTX_HASH = "__noctx__";

/**
 * Default `placeholderData` for every query whose queryKey embeds a
 * runtime parameter (branch list, base ref, PR-number list, …). When
 * the parameter shifts (worktree added/removed/renamed, stack parent
 * flips, PR set churns), the observer switches to a different cache
 * entry; without this, the new entry's `data` is `undefined` until
 * the fetch lands and every dependent badge / row blanks together.
 * `keepPreviousData` keeps the prior entry's value on screen across
 * the flip so the UI stays painted. Stable-key queries don't need
 * this — TanStack already retains data across refetches when the
 * key doesn't change. `aiSummaryQuery` opts in at the consumer
 * instead (see its docstring for the cross-slug hazard).
 */
export const KEEP_PREV = { placeholderData: keepPreviousData } as const;

// ---------- Stale-time policy ----------
// Short for cheap fs-backed queries; longer for network/git-heavy ones.
export const STALE = {
  fast: 5_000, // fs checks (dirty, lock, deploy)
  mid: 15_000, // listWorktrees, branchIsMerged
  slow: 60_000, // PR fetch, fetchOrigin, firstParents
} as const;
