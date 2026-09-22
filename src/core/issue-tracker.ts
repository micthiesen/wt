import { config } from "./config.ts";

// Input conveniences for `wt new`: a pasted URL or bare id resolves to
// a branch. Independent of `[issue_tracker]` — they parse *input*, not
// config. The URL shape is Linear's (the one tracker with a known
// paste-a-URL format); the bare-id shape matches any Linear/Jira/
// Linear/Jira/Shortcut-style id (`ENG-1234`, `ABC-1953`).
export const ISSUE_URL_RE = /linear\.app\/[^/]+\/issue\/([A-Z]+-\d+)/i;
export const ISSUE_ID_RE = /^[A-Z]+-\d+$/i;

/** Issue id embedded in a worktree slug (`coz-1883-some-fix` → `coz-1883`). */
export const ISSUE_SLUG_ID_RE = /([a-z]+-\d+)(?:-|$)/i;

/** GitHub notes are never tracker tasks; an optional prefix scopes the provider. */
export function isTrackerIssueId(id: string | null, prefix: string | null = config.issueTracker?.prefix ?? null): boolean {
  return !!id && ISSUE_ID_RE.test(id) && !/^GH-\d+$/i.test(id) &&
    (!prefix || id.toUpperCase().startsWith(`${prefix.toUpperCase()}-`));
}

/**
 * Uppercased issue id parsed from a slug, or null when the slug carries
 * none. Independent of `[issue_tracker]` — parsing is free; config only
 * decides whether/where the id is displayed or linked.
 */
export function issueIdForSlug(slug: string): string | null {
  const m = ISSUE_SLUG_ID_RE.exec(slug);
  return m?.[1] ? m[1].toUpperCase() : null;
}

/**
 * A `GH-<n>` id refers to a GitHub issue on this repo — a built-in
 * convention, not a tracker prefix (GitHub has no team-id prefixes of
 * its own, so `gh-970` in a slug is unambiguous). It routes to the
 * origin repo's /issues/<n> instead of `url_template`.
 */
const GH_ID_RE = /^GH-(\d+)$/;

/**
 * Web URL for a git remote (`git@github.com:o/r.git`,
 * `https://github.com/o/r.git`, `ssh://git@github.com/o/r` →
 * `https://github.com/o/r`). Null when the shape isn't recognized —
 * bare host aliases (`work:o/r.git`) don't name a real host, so no URL
 * is derivable. Exported for tests.
 */
export function repoWebUrl(remote: string): string | null {
  const m =
    /^(?:git@|ssh:\/\/git@|https?:\/\/)([^/:]+\.[^/:]+)[/:]([^/]+)\/([^/]+?)(?:\.git)?\/?$/.exec(
      remote.trim(),
    );
  return m ? `https://${m[1]}/${m[2]}/${m[3]}` : null;
}

/** Memoized origin-remote web URL of the main clone (null = underivable). */
let _mainRepoWebUrl: string | null | undefined;
function mainRepoWebUrl(): string | null {
  if (_mainRepoWebUrl === undefined) {
    const r = Bun.spawnSync(
      ["git", "-C", config.paths.mainClone, "remote", "get-url", "origin"],
    );
    _mainRepoWebUrl =
      r.exitCode === 0 ? repoWebUrl(r.stdout.toString()) : null;
  }
  return _mainRepoWebUrl;
}

/**
 * The worktree's tracker id: the stored OVERRIDE when one is set,
 * else the id parsed from the slug.
 *
 * Every reader of "which issue is this worktree" goes through here, so
 * a worktree whose slug carries no id (`camera-selection-sticky`) can
 * still have one — which is the whole point, since that population is
 * the common case rather than the exception (0 of 6 live rows carried
 * a slug id on 2026-08-21) and it is what `{{issue_id}}` renders,
 * what `requires = ["issue.tracker"]` tests, and what the tracker
 * automation moves to In Review.
 *
 * Pure: the caller supplies the stored value (from wtstate), the same
 * way `preferredIssueUrl` takes `githubIssue`. That keeps this callable
 * from the automations evaluator and the action-requirement check,
 * neither of which may do I/O.
 */
export function resolveIssueId(
  slug: string,
  stored: string | null | undefined,
): string | null {
  // Three states, not two. ABSENT (null/undefined) means "nothing
  // stored, use the slug". The EMPTY STRING means "this worktree has
  // no tracker issue" — an asserted none, which a slug carrying an id
  // would otherwise override forever: before it existed, clearing on
  // `coz-2101-connector-research` fell straight back to COZ-2101 and
  // there was no way to say the work is not that ticket.
  // Any all-whitespace string, not just `""`: the store trims on the
  // way in, but imported/corrupt durable state can carry `"  "`, and letting
  // that fall through would quietly resurrect the slug's id under an
  // override whose whole purpose was to suppress it.
  if (typeof stored === "string" && stored.trim() === "") return null;
  const s = stored?.trim();
  if (s) return s.toUpperCase();
  return issueIdForSlug(slug);
}

/**
 * Deep link for a tracker id. `GH-<n>` ids link to the origin repo's
 * GitHub issue; everything else goes through
 * `[issue_tracker].url_template` (or the `[issue_tracker.linear]`
 * preset). Null when no link is derivable or the id is absent.
 */
export function issueUrlForId(id: string | null): string | null {
  if (!id) return null;
  const gh = GH_ID_RE.exec(id);
  if (gh) {
    const repo = mainRepoWebUrl();
    return repo ? `${repo}/issues/${gh[1]}` : null;
  }
  const template = config.issueTracker?.urlTemplate;
  if (!template) return null;
  return template.replaceAll("{id}", id);
}

/**
 * Deep link for the id carried in the SLUG, ignoring any stored
 * override. Only for callers with no access to wtstate; anything
 * holding a row or a state entry uses `issueUrlForId(resolveIssueId(…))`.
 */
export function issueUrlForSlug(slug: string): string | null {
  return issueUrlForId(issueIdForSlug(slug));
}

/** Web URL for a GitHub issue number on the origin repo (null = repo underivable). */
export function githubIssueUrl(issue: number): string | null {
  const repo = mainRepoWebUrl();
  return repo ? `${repo}/issues/${issue}` : null;
}

/**
 * GitHub issue number when the slug's PRIMARY id is a `GH-<n>` id
 * (`gh-970-fix-tabs` → 970), null otherwise. Pure — safe from the
 * automations evaluator, which uses it as the fallback identity when
 * no secondary `--gh` issue is attached.
 */
export function githubIssueNumberFromSlug(slug: string): number | null {
  const id = issueIdForSlug(slug);
  const m = id ? GH_ID_RE.exec(id) : null;
  return m ? Number(m[1]) : null;
}

/**
 * The row's default link target: the primary tracker issue when linkable,
 * otherwise the attached GitHub issue. Shared by `i` and `y i`;
 * `I` / `y I` remain primary-only, without the secondary fallback.
 */
export function preferredIssueUrl(
  slug: string,
  githubIssue: number | null | undefined,
  storedIssueId?: string | null,
): string | null {
  const tracker = issueUrlForId(resolveIssueId(slug, storedIssueId));
  return tracker ?? (githubIssue ? githubIssueUrl(githubIssue) : null);
}
