import { config } from "../config.ts";
import { createLogger } from "../logger.ts";
import { run } from "../proc.ts";
import type { Contributor } from "../types.ts";
import { hasGh, repoSlug } from "./gh-cli.ts";

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
async function fetchActiveCommitAuthors(signal?: AbortSignal): Promise<Set<string>> {
  const empty = new Set<string>();
  if (!(await hasGh())) return empty;
  const slug = await repoSlug();
  if (!slug) return empty;
  const since = new Date(Date.now() - CONTRIB_RECENCY_MS).toISOString();
  const authors = new Set<string>();
  for (let page = 1; page <= ACTIVE_AUTHORS_MAX_PAGES; page++) {
    const r = await run(
      [
        "gh",
        "api",
        `repos/${slug}/commits?since=${since}&per_page=100&page=${page}`,
      ],
      { cwd: config.paths.mainClone, timeoutMs: 15_000, signal },
    );
    if (r.exitCode !== 0) {
      log.error("active authors fetch failed", {
        stderr: r.stderr.slice(0, 200),
        page,
      });
      return empty;
    }
    let arr: Array<{ author: { login?: string } | null }> = [];
    try {
      arr = JSON.parse(r.stdout);
    } catch (err) {
      log.error(err instanceof Error ? err : String(err), { page });
      return empty;
    }
    if (arr.length === 0) break;
    for (const c of arr) {
      if (c.author?.login) authors.add(c.author.login);
    }
    if (arr.length < 100) break;
  }
  return authors;
}

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
 * Returns [] on any failure — same posture as `fetchGithub`. If only
 * the recency check fails (but contributors succeeded), we return
 * the unfiltered contributors so the picker isn't empty.
 */
export async function fetchRepoContributors(signal?: AbortSignal): Promise<Contributor[]> {
  if (!(await hasGh())) return [];
  const slug = await repoSlug();
  if (!slug) return [];
  const [contribRes, activeAuthors] = await Promise.all([
    run(["gh", "api", `repos/${slug}/contributors?per_page=100`], {
      cwd: config.paths.mainClone,
      timeoutMs: 15_000,
      signal,
    }),
    fetchActiveCommitAuthors(signal),
  ]);
  if (contribRes.exitCode !== 0) {
    log.error("contributors fetch failed", {
      stderr: contribRes.stderr.slice(0, 200),
      exitCode: contribRes.exitCode,
    });
    return [];
  }
  let arr: Array<{
    login?: string;
    type?: string;
    contributions?: number;
  }>;
  try {
    arr = JSON.parse(contribRes.stdout);
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), {
      stdout: contribRes.stdout.slice(0, 200),
    });
    return [];
  }
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
}
