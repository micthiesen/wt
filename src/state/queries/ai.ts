import { createHash } from "node:crypto";

import { queryOptions } from "@tanstack/react-query";

import { summarizeDiff, summarizeStack, type AiSummary } from "../../core/ai.ts";
import { config } from "../../core/config.ts";
import type { DiffContext } from "../../core/diff/index.ts";
import { buildDiffContextViaPool } from "../../core/diff/pool.ts";
import type {
  Worktree,
} from "../../core/types.ts";
import { createLogger } from "../../core/logger.ts";
import { pluralize } from "../../core/text.ts";

import { qk } from "../keys.ts";
import { KEEP_PREV, NO_CTX_HASH, STALE } from "./shared.ts";

const aiLog = createLogger("ai");

/**
 * Diff context + content hash for the AI summary. The hash is the
 * stable cache key for `aiSummaryQuery`; the prompt body lives only in
 * memory (not serialised to the cache, since it can be megabytes).
 * Local + fast — silent in normal operation, like the other per-wt
 * git queries.
 *
 * `effectiveBase` defaults to `origin/<config.branch.base>` (trunk).
 * For stacked worktrees the row aggregator passes the parent's branch
 * instead so the diff reflects only this PR's contribution. The query
 * key includes the base so a base flip triggers a refetch via cache
 * miss rather than relying on invalidation.
 */
export const wtDiffContextQuery = (
  wt: Pick<Worktree, "slug" | "path">,
  effectiveBase?: string | null,
) => {
  const base = effectiveBase ?? `origin/${config.branch.base}`;
  return queryOptions({
    queryKey: qk.wt(wt.slug).diffContext(base),
    queryFn: async ({ signal }): Promise<DiffContext | null> =>
      buildDiffContextViaPool(wt.path, base, signal),
    staleTime: STALE.mid,
    ...KEEP_PREV,
  });
};

/**
 * AI-generated summary of the diff, keyed by the diff's content hash.
 * Equivalent diffs across rebases / amends / branch renames hit the
 * same cache entry — that's the whole point of content-addressed
 * keying.
 *
 * The "keep the previous summary visible while a new hash is loading"
 * behavior is the consumer's job: pair this with
 * `placeholderData: keepPreviousData` so a hash flip (diff changed)
 * doesn't blank the description during the gap.
 *
 * Cross-slug hazard: in a `useQuery` consumer (single observer that
 * survives subject changes), `keepPreviousData` will leak the prior
 * slug's summary into the new slug whenever the new slug's queryKey
 * has no cache entry — including the `__noctx__` empty-branch case,
 * which is `enabled: false` so nothing ever overwrites the placeholder.
 * Scope the observer to one slug (e.g. `key={slug}` on the consuming
 * component) so it remounts on slug change. `useQueries` consumers are
 * safe — `QueriesObserver` matches observers by queryHash, so each
 * hash gets its own observer regardless of array position.
 *
 * Pass `null` for `ctx` when the diff context isn't ready; pair with
 * `enabled: !!ctx` so the queryFn never runs and the early `null`
 * sentinel is just a type accommodation.
 */
export const aiSummaryQuery = (
  slug: string,
  ctx: { hash: string; prompt: string } | null,
) =>
  queryOptions({
    // `slug` doesn't participate in the cache key — that's intentional,
    // it's only here for the activity log line. Two worktrees with
    // identical diffs share an entry; the log shows whichever slug
    // triggered the fetch.
    queryKey: qk.aiSummary(ctx?.hash ?? NO_CTX_HASH),
    queryFn: async ({ signal }): Promise<AiSummary> => {
      // The `enabled: !!ctx` guard at the call site makes this branch
      // unreachable. We throw rather than caching `null` defensively:
      // a `null` entry under `NO_CTX_HASH` with `staleTime: Infinity`
      // would be a forever-stuck "no summary" if this ever fired.
      if (!ctx) {
        throw new Error("aiSummaryQuery: ctx is null (enabled guard missed)");
      }
      aiLog.event.dim(`calling AI endpoint for ${slug} (${pluralize(ctx.prompt.length, "char")})...`);
      const start = Date.now();
      try {
        const out = await summarizeDiff(ctx.prompt, signal);
        aiLog.event.dim(`called AI endpoint for ${slug} (${formatDuration(Date.now() - start)})`);
        return out;
      } catch (err) {
        // A cancelled observer (diff hash flipped again, row unmounted)
        // aborts the in-flight call — routine supersession, not a
        // failure worth an activity-pane line.
        if (signal.aborted) throw err;
        const msg = err instanceof Error ? err.message : String(err);
        aiLog.event.err(
          `AI endpoint failed for ${slug} (${formatDuration(Date.now() - start)}): ${msg}`,
        );
        throw err;
      }
    },
    // Hash-keyed: a new diff produces a new cache entry. No staleness
    // policy needed within an entry — the diff content can't change
    // without producing a different hash.
    staleTime: Number.POSITIVE_INFINITY,
    gcTime: Number.POSITIVE_INFINITY,
  });

function formatDuration(ms: number): string {
  return ms < 1000 ? `${ms}ms` : `${(ms / 1000).toFixed(1)}s`;
}

export type StackMember = { branch: string; brief: string };

/**
 * Stable signature for a stack: a hash over the sorted *branch names*
 * only. Briefs are passed to the LLM as flavor but deliberately don't
 * participate in the cache key, so:
 *
 *   - Cold start (briefs not loaded yet) → signature stable; restored
 *     persisted title appears immediately without a wasted refetch.
 *   - A member's commits change (brief regenerates) → signature
 *     unchanged; title sticks. Stack themes rarely pivot per-commit,
 *     so this is the right default. A manual regen knob lives below
 *     for the "title is wrong, redo it" case.
 *   - Member set changes (branch added / removed from the chain) →
 *     signature flips → fresh title fetched.
 *
 * Sentinel `__empty__` for an empty list pairs with the `enabled`
 * guard so the queryFn never runs against it.
 *
 * The `v2` salt orphans every title generated before the briefs-ready
 * gate existed: those fired with slug-fallback briefs and cached
 * prompt-leakage junk ("TUI Header Orchestration Stack") forever.
 * Orphaned entries age out of the persister via maxAge.
 */
export function buildStackSignature(
  members: ReadonlyArray<StackMember>,
): string {
  if (members.length === 0) return "__empty__";
  const branches = members.map((m) => m.branch).sort();
  return createHash("sha256")
    .update(["v2", ...branches].join("\0"))
    .digest("hex")
    .slice(0, 16);
}

/**
 * AI-named stack section title. Hash-keyed on the member-branch
 * signature (see `buildStackSignature` for why briefs are excluded
 * from the key) so two stacks with the same membership share one
 * cache entry. Member additions / removals cut a fresh entry; commit
 * churn within members does not.
 *
 * `sectionName` is passed through for the activity log line only; it
 * doesn't participate in the cache key.
 *
 * Persisted: falls into the persister's default-true branch (key
 * length < 3) so the entry survives TUI restarts; restored entries
 * skip the queryFn on first observe, no LM call needed until the
 * member set changes.
 */
export const stackTitleQuery = (
  sectionName: string,
  members: ReadonlyArray<StackMember>,
) =>
  queryOptions({
    queryKey: qk.stackTitle(buildStackSignature(members)),
    queryFn: async ({ signal }): Promise<string> => {
      if (members.length === 0) {
        throw new Error("stackTitleQuery: members empty (enabled guard missed)");
      }
      aiLog.event.dim(`naming stack ${sectionName} (${members.length} members)...`);
      const start = Date.now();
      try {
        const title = await summarizeStack(members, signal);
        aiLog.event.dim(
          `named stack ${sectionName} → "${title}" (${formatDuration(Date.now() - start)})`,
        );
        return title;
      } catch (err) {
        // Same cancellation gate as aiSummaryQuery: an aborted signal
        // is supersession, not a failure.
        if (signal.aborted) throw err;
        const msg = err instanceof Error ? err.message : String(err);
        aiLog.event.err(
          `naming stack ${sectionName} failed (${formatDuration(Date.now() - start)}): ${msg}`,
        );
        throw err;
      }
    },
    enabled: members.length > 0 && !!config.ai,
    staleTime: Number.POSITIVE_INFINITY,
    gcTime: Number.POSITIVE_INFINITY,
  });
