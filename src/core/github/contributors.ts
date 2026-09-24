import { Clock, DateTime, Effect } from "effect";

import { config } from "../config.ts";
import { createLogger } from "../logger.ts";
import { run } from "../proc.ts";
import type { Contributor } from "../types.ts";
import { GH_TIMEOUT_MS, ghFailureMessage, hasGh, repoSlug } from "./gh-cli.ts";

const log = createLogger("[gh]");

const CONTRIB_RECENCY_MS = 6 * 30 * 24 * 60 * 60 * 1000; // ~6 months
const ACTIVE_AUTHORS_MAX_PAGES = 5; // ≈ up to 500 commits

/**
 * Logins that committed to the default branch in the last
 * `CONTRIB_RECENCY_MS`. Used to drop stale entries (former
 * contractors etc.) from the all-time contributors list. Returns an
 * empty set on failure; callers should treat empty as "don't filter"
 * rather than "everyone is inactive".
 */
const fetchActiveCommitAuthors = Effect.fnUntraced(function* (
  slug: string,
  now: number,
  signal?: AbortSignal,
): Effect.fn.Return<Set<string>> {
  const empty = new Set<string>();
  const since = DateTime.formatIso(DateTime.makeUnsafe(now - CONTRIB_RECENCY_MS));
  const authors = new Set<string>();
  for (let page = 1; page <= ACTIVE_AUTHORS_MAX_PAGES; page++) {
    const r = yield* run(
      [
        "gh",
        "api",
        `repos/${slug}/commits?since=${since}&per_page=100&page=${page}`,
      ],
      { cwd: config.paths.mainClone, timeoutMs: GH_TIMEOUT_MS, signal },
    ).pipe(Effect.catch(() => Effect.succeed(null)));
    if (r === null) return empty;
    if (r.exitCode !== 0) {
      log.error("active authors fetch failed", {
        error: ghFailureMessage(r).slice(0, 200),
        page,
      });
      return empty;
    }
    const arr = yield* Effect.try(
      () => JSON.parse(r.stdout) as Array<{ author: { login?: string } | null }>,
    ).pipe(
      Effect.catch((err) => {
        log.error(err instanceof Error ? err : String(err), { page });
        return Effect.succeed(null);
      }),
    );
    if (!arr) return empty;
    if (arr.length === 0) break;
    for (const c of arr) {
      if (c.author?.login) authors.add(c.author.login);
    }
    if (arr.length < 100) break;
  }
  return authors;
});

/**
 * Fetch the top-100 most-active human contributors for the repo via
 * the REST contributors endpoint, then intersect with the set of
 * logins that have committed in the last ~6 months. The intersection
 * drops stale all-time-but-not-recent entries (former contractors
 * etc.). Bots are filtered out so the picker doesn't surface
 * `dependabot[bot]` as a viable reviewer. The fallback list for
 * `editReviewers` when GitHub's per-PR `suggestedReviewers` is empty,
 * which it often is on small/focused diffs.
 *
 * Returns [] on any failure — deliberately laxer than `fetchGithub`
 * (which throws so transient blips can't blank cached PR state): an
 * empty reviewer picker is low-stakes and self-heals on reopen. If only
 * the recency check fails (but contributors succeeded), we return
 * the unfiltered contributors so the picker isn't empty.
 */
export const fetchRepoContributors = Effect.fn("fetchRepoContributors")(
  function* (signal?: AbortSignal): Effect.fn.Return<Contributor[]> {
    if (!(yield* hasGh())) return [];
    const slug = yield* repoSlug();
    if (!slug) return [];
    const now = yield* Clock.currentTimeMillis;
    const [contribRes, activeAuthors] = yield* Effect.all([
      run(["gh", "api", `repos/${slug}/contributors?per_page=100`], {
        cwd: config.paths.mainClone,
        timeoutMs: GH_TIMEOUT_MS,
        signal,
      }).pipe(Effect.catch(() => Effect.succeed(null))),
      fetchActiveCommitAuthors(slug, now, signal),
    ], { concurrency: 2 });
    if (contribRes === null) return [];
    if (contribRes.exitCode !== 0) {
      log.error("contributors fetch failed", {
        error: ghFailureMessage(contribRes).slice(0, 200),
        exitCode: contribRes.exitCode,
      });
      return [];
    }
    const arr = yield* Effect.try(() => JSON.parse(contribRes.stdout) as Array<{
      login?: string;
      type?: string;
      contributions?: number;
    }>).pipe(
      Effect.catch((err) => {
        log.error(err instanceof Error ? err : String(err), {
          stdout: contribRes.stdout.slice(0, 200),
        });
        return Effect.succeed(null);
      }),
    );
    if (!arr) return [];
    const out: Contributor[] = [];
    for (const c of arr) {
      if (!c.login) continue;
      // GitHub flags bots with `type: "Bot"`, but some apps slip
      // through as "User" with a `[bot]` login suffix.
      if (c.type === "Bot") continue;
      if (c.login.endsWith("[bot]")) continue;
      // If we have an active-authors set, require membership; otherwise
      // fall back to unfiltered so a recency-check failure doesn't empty
      // the picker.
      if (activeAuthors.size > 0 && !activeAuthors.has(c.login)) continue;
      out.push({ login: c.login, contributions: c.contributions ?? 0 });
    }
    return out;
  },
);
