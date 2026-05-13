import { statSync } from "node:fs";

import { config } from "./config.ts";
import { createLogger } from "./logger.ts";
import { run } from "./proc.ts";
import type {
  Contributor,
  LatestReview,
  PrChecks,
  PrReview,
  PullRequest,
  RabbitStatus,
  Worktree,
} from "./types.ts";
import { listWorktrees } from "./worktree.ts";

const log = createLogger("[gh]");

async function hasGh(): Promise<boolean> {
  const r = await run(["which", "gh"]);
  return r.exitCode === 0 && r.stdout.trim().length > 0;
}

// Cache the resolved `owner/name` — it never changes for a given clone.
let _repoSlug: string | null | undefined;
export async function repoSlug(): Promise<string | null> {
  if (_repoSlug !== undefined) return _repoSlug;
  const r = await run(
    ["gh", "repo", "view", "--json", "nameWithOwner"],
    { cwd: config.paths.mainClone, timeoutMs: 5_000 },
  );
  if (r.exitCode !== 0) {
    _repoSlug = null;
    return null;
  }
  try {
    const data = JSON.parse(r.stdout) as { nameWithOwner?: string };
    _repoSlug = data.nameWithOwner ?? null;
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { stdout: r.stdout.slice(0, 200) });
    _repoSlug = null;
  }
  return _repoSlug;
}

type RawCheck =
  | { __typename: "CheckRun"; name?: string | null; status?: string | null; conclusion?: string | null }
  | { __typename: "StatusContext"; context?: string | null; state?: string | null };

const CHECK_FAIL_CONCLUSIONS = new Set([
  "FAILURE",
  "CANCELLED",
  "TIMED_OUT",
  "ACTION_REQUIRED",
  "STARTUP_FAILURE",
]);

/** Compile `[github] ignored_checks` (case-insensitive globs, `*` only) into a predicate. */
function compileIgnore(patterns: readonly string[]): (name: string | null | undefined) => boolean {
  if (patterns.length === 0) return () => false;
  const regexes = patterns.map((p) => {
    const escaped = p.replace(/[.+?^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*");
    return new RegExp(`^${escaped}$`, "i");
  });
  return (name) => {
    if (!name) return false;
    return regexes.some((r) => r.test(name));
  };
}

const isIgnoredCheck = compileIgnore(config.github.ignoredChecks);

function checkName(c: RawCheck): string | null | undefined {
  return c.__typename === "CheckRun" ? c.name : c.context;
}

function rollupChecks(raw: RawCheck[] | null | undefined): PrChecks {
  if (!raw || raw.length === 0) return "none";
  let pending = false;
  let fail = false;
  let counted = 0;
  for (const c of raw) {
    if (isIgnoredCheck(checkName(c))) continue;
    counted++;
    if (c.__typename === "CheckRun") {
      if (c.status && c.status !== "COMPLETED") pending = true;
      else if (c.conclusion && CHECK_FAIL_CONCLUSIONS.has(c.conclusion)) fail = true;
    } else {
      const s = c.state;
      if (s === "PENDING" || s === "EXPECTED") pending = true;
      else if (s === "FAILURE" || s === "ERROR") fail = true;
    }
  }
  if (counted === 0) return "none";
  if (fail) return "fail";
  if (pending) return "pending";
  return "pass";
}

// Shared fields for each PR. Used by every aliased sub-query below.
const PR_FRAGMENT = `
fragment PrFields on PullRequest {
  number
  url
  title
  headRefName
  baseRefName
  isDraft
  state
  mergedAt
  closedAt
  reviewDecision
  reviewRequests(first: 20) {
    totalCount
    nodes {
      requestedReviewer {
        __typename
        ... on User { login }
        ... on Team { combinedSlug }
      }
    }
  }
  suggestedReviewers {
    reviewer { login }
    isAuthor
    isCommenter
  }
  commits(last: 1) {
    nodes {
      commit {
        statusCheckRollup {
          contexts(first: 50) {
            nodes {
              __typename
              ... on CheckRun { name status conclusion }
              ... on StatusContext { context state }
            }
          }
        }
      }
    }
  }
  reviewThreads(first: 50) {
    nodes {
      isResolved
      comments(first: 1) {
        nodes { author { login } }
      }
    }
  }
  reviews(last: 10) {
    nodes {
      author { login }
      body
      state
    }
  }
}`;

// CodeRabbit's status-check context name and GraphQL author login. Both
// are user-facing strings owned by CR, not us — if they change, the
// rabbit badge silently disappears (state: "none") rather than breaking
// the whole pane. Hardcoded by design; mirrors `~/.claude/skills/rabbit`.
const CR_CONTEXT = "CodeRabbit";
const CR_LOGIN = "coderabbitai";

type GqlReviewThread = {
  isResolved: boolean;
  comments: { nodes: Array<{ author: { login: string | null } | null }> };
};

function rollupRabbit(
  state: PullRequest["state"],
  contexts: RawCheck[] | null | undefined,
  threads: GqlReviewThread[] | null | undefined,
): RabbitStatus {
  if (state !== "OPEN") return { state: "none", unresolved: 0 };

  let unresolved = 0;
  for (const t of threads ?? []) {
    if (t.isResolved) continue;
    if (t.comments.nodes[0]?.author?.login === CR_LOGIN) unresolved++;
  }
  // Unresolved feedback takes precedence over a fresh re-run — pushes
  // re-trigger CR routinely and the old threads are still what needs
  // addressing.
  if (unresolved > 0) return { state: "unresolved", unresolved };

  // Find CR's status-check context to distinguish "still running" from
  // "done with no findings" from "never ran / not configured".
  let crContextState: "pending" | "done" | null = null;
  for (const c of contexts ?? []) {
    const name = c.__typename === "CheckRun" ? c.name : c.context;
    if (name !== CR_CONTEXT) continue;
    if (c.__typename === "CheckRun") {
      crContextState = c.status && c.status !== "COMPLETED" ? "pending" : "done";
    } else {
      crContextState = c.state === "PENDING" || c.state === "EXPECTED" ? "pending" : "done";
    }
    break;
  }
  if (crContextState === "pending") return { state: "pending", unresolved: 0 };
  if (crContextState === "done") return { state: "clean", unresolved: 0 };
  return { state: "none", unresolved: 0 };
}

/**
 * Build a graphql doc with one aliased `pullRequests(headRefName:)`
 * sub-query per branch. `first: 2` per branch catches the rare
 * "branch has a reopen" case where there's an OPEN and a terminal PR
 * on the same ref — we'll prefer OPEN at parse time.
 *
 * Scoping to exact branches rather than pulling the 100 most recent
 * drops wall-clock ~4x and response size ~8x at 10 worktrees; it also
 * means the query cost is bounded by the number of worktrees, not by
 * how busy the repo is.
 */
function buildQuery(branchCount: number): string {
  if (branchCount === 0) {
    // Graphql rejects an empty selection set; a noop `__typename` plus
    // no fragment keeps the round-trip well-formed in the degenerate
    // "no worktrees" case.
    return `query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) { __typename }
}`;
  }
  const varDecls = Array.from({ length: branchCount }, (_, i) => `$b${i}: String!`).join(", ");
  const aliases = Array.from({ length: branchCount }, (_, i) =>
    `    wt_${i}: pullRequests(first: 2, headRefName: $b${i}, orderBy: {field: UPDATED_AT, direction: DESC}) { nodes { ...PrFields } }`,
  ).join("\n");
  return `
query($owner: String!, $name: String!, ${varDecls}) {
  repository(owner: $owner, name: $name) {
${aliases}
  }
}
${PR_FRAGMENT}`;
}

type GqlReviewDecision = "APPROVED" | "CHANGES_REQUESTED" | "REVIEW_REQUIRED" | null;

type GqlPrNode = {
  number: number;
  url: string;
  title: string;
  headRefName: string;
  baseRefName: string;
  isDraft: boolean;
  state: PullRequest["state"];
  mergedAt: string | null;
  closedAt: string | null;
  reviewDecision: GqlReviewDecision;
  reviewRequests: {
    totalCount: number;
    nodes: Array<{
      requestedReviewer:
        | { __typename: "User"; login: string }
        | { __typename: "Team"; combinedSlug: string }
        | null;
    }>;
  } | null;
  suggestedReviewers: Array<{
    reviewer: { login: string } | null;
    isAuthor: boolean;
    isCommenter: boolean;
  }> | null;
  commits: {
    nodes: Array<{
      commit: {
        statusCheckRollup: { contexts: { nodes: RawCheck[] } } | null;
      };
    }>;
  };
  reviewThreads: { nodes: GqlReviewThread[] } | null;
  reviews: {
    nodes: Array<{
      author: { login: string | null } | null;
      body: string;
      state: GqlReviewSubmissionState;
    }>;
  } | null;
};

type GqlReviewSubmissionState =
  | "APPROVED"
  | "CHANGES_REQUESTED"
  | "COMMENTED"
  | "DISMISSED"
  | "PENDING";

function rollupReview(
  state: PullRequest["state"],
  decision: GqlReviewDecision,
  outstandingRequests: number,
  hasStaleChangesRequest: boolean,
): PrReview {
  // Terminal PRs don't carry useful review state; suppress to keep the
  // line uncluttered. The PR badge already conveys merged/closed.
  if (state !== "OPEN") return "none";
  if (decision === "APPROVED") return "approved";
  if (decision === "CHANGES_REQUESTED") {
    // GitHub keeps `reviewDecision` pinned at CHANGES_REQUESTED until
    // the same reviewer submits a new review or the old one is
    // dismissed — re-requesting review doesn't clear it. When the
    // reviewer who CR'd is back in `reviewRequests`, the practical
    // state is "fixed it, asked again, waiting" so surface that as
    // pending instead of leaving the stale "changes requested" badge
    // up.
    if (hasStaleChangesRequest) return "pending";
    return "changes_requested";
  }
  // Distinguish "nobody asked yet" from "asked, waiting" — the action
  // for the first is to hit `v`, for the second it's to wait.
  if (outstandingRequests === 0) return "unrequested";
  return "pending";
}

/**
 * True when at least one reviewer whose latest review was
 * CHANGES_REQUESTED has been re-requested (their login is back in the
 * currently-requested set). Walks `reviews` newest-to-oldest and only
 * keeps each author's most recent terminal verdict so a CR followed by
 * a fresh APPROVED doesn't keep flagging the author.
 */
function hasStaleChangesRequest(
  reviews: GqlPrNode["reviews"],
  currentlyRequested: readonly string[],
): boolean {
  if (currentlyRequested.length === 0) return false;
  const requested = new Set(currentlyRequested);
  const seen = new Set<string>();
  const nodes = reviews?.nodes ?? [];
  for (let i = nodes.length - 1; i >= 0; i--) {
    const r = nodes[i];
    const author = r?.author?.login;
    if (!author || seen.has(author)) continue;
    // Only verdict-bearing states reset the author's running state.
    // COMMENTED and PENDING are non-terminal and should be skipped so
    // an idle "commented" after a CR doesn't mask the CR.
    if (r.state !== "APPROVED" && r.state !== "CHANGES_REQUESTED" && r.state !== "DISMISSED") {
      continue;
    }
    seen.add(author);
    if (r.state === "CHANGES_REQUESTED" && requested.has(author)) return true;
  }
  return false;
}

type GqlRepo = {
  // Each aliased `wt_N` key lands here as `{ nodes: GqlPrNode[] }`.
  [alias: `wt_${number}`]: { nodes?: GqlPrNode[] } | undefined;
};

type GqlResponse = {
  data?: { repository?: GqlRepo };
};

export type GithubData = {
  prs: Map<string, PullRequest>;
};

/**
 * A pull request the authenticated user has been asked to review. Not a
 * worktree (we typically don't have a local checkout of someone else's
 * branch), just a pinned list at the bottom of the TUI. Carries the
 * minimum surface needed for the list label, the lite details pane, and
 * the `p` open-in-Graphite action.
 */
export type ReviewRequestPr = {
  number: number;
  url: string;
  title: string;
  repoNameWithOwner: string;
  author: string | null;
  isDraft: boolean;
  checks: PrChecks;
  createdAt: string;
  updatedAt: string;
};

function extractRequestedReviewers(
  rr: GqlPrNode["reviewRequests"],
): string[] {
  const out: string[] = [];
  for (const node of rr?.nodes ?? []) {
    const r = node.requestedReviewer;
    if (!r) continue;
    if (r.__typename === "User") out.push(r.login);
    else if (r.__typename === "Team") out.push(r.combinedSlug);
  }
  return out;
}

function extractSuggestedReviewers(
  sr: GqlPrNode["suggestedReviewers"],
): import("./types.ts").SuggestedReviewer[] {
  const out: import("./types.ts").SuggestedReviewer[] = [];
  for (const s of sr ?? []) {
    if (!s.reviewer?.login) continue;
    out.push({
      login: s.reviewer.login,
      isAuthor: s.isAuthor,
      isCommenter: s.isCommenter,
    });
  }
  return out;
}

/**
 * Most recent human review with a non-empty body. Walks newest-to-oldest
 * (graphql `reviews(last:N)` orders ascending, so the freshest entry is
 * at the end of the array) and skips empties, missing authors, and
 * CodeRabbit — its activity has its own badge / unresolved-thread count
 * and would otherwise drown out the human review message.
 */
function extractLatestReview(
  reviews: GqlPrNode["reviews"],
): LatestReview | null {
  const nodes = reviews?.nodes ?? [];
  for (let i = nodes.length - 1; i >= 0; i--) {
    const r = nodes[i];
    const body = r?.body?.trim();
    const author = r?.author?.login;
    if (!body || !author) continue;
    if (author === CR_LOGIN) continue;
    return { author, body };
  }
  return null;
}

function nodeToPr(pr: GqlPrNode): PullRequest {
  const contexts =
    pr.commits.nodes[0]?.commit?.statusCheckRollup?.contexts?.nodes ?? null;
  const threads = pr.reviewThreads?.nodes ?? null;
  const requestedReviewers = extractRequestedReviewers(pr.reviewRequests);
  return {
    number: pr.number,
    url: pr.url,
    title: pr.title,
    headRefName: pr.headRefName,
    baseRefName: pr.baseRefName,
    isDraft: pr.isDraft,
    state: pr.state,
    checks: rollupChecks(contexts),
    review: rollupReview(
      pr.state,
      pr.reviewDecision,
      pr.reviewRequests?.totalCount ?? 0,
      hasStaleChangesRequest(pr.reviews, requestedReviewers),
    ),
    reviewRequests: pr.reviewRequests?.totalCount ?? 0,
    rabbit: rollupRabbit(pr.state, contexts, threads),
    requestedReviewers,
    suggestedReviewers: extractSuggestedReviewers(pr.suggestedReviewers),
    latestReview: extractLatestReview(pr.reviews),
    mergedAt: pr.mergedAt ?? null,
    closedAt: pr.closedAt ?? null,
  };
}

/**
 * Fetch PRs for a fixed set of branches in a single graphql round
 * trip. Returns an empty map on any failure — the TUI treats "no
 * entry" as "not there" rather than surfacing transient errors. Pass
 * the exact worktree branches; anything not on the list is never
 * fetched (the TUI wouldn't display it anyway).
 *
 * `signal` (when provided) cascades into the underlying `gh` invocation
 * so a superseded query — branch list re-keyed before the previous
 * fetch returned — actually stops the subprocess instead of letting it
 * burn a graphql round trip on data nobody will read.
 */
export async function fetchGithub(
  branches: string[],
  signal?: AbortSignal,
): Promise<GithubData> {
  const empty: GithubData = { prs: new Map() };
  if (!(await hasGh())) return empty;
  const slug = await repoSlug();
  if (!slug) return empty;
  const [owner, name] = slug.split("/");
  if (!owner || !name) return empty;

  const query = buildQuery(branches.length);
  const args = [
    "gh",
    "api",
    "graphql",
    "-f",
    `query=${query}`,
    "-f",
    `owner=${owner}`,
    "-f",
    `name=${name}`,
  ];
  for (let i = 0; i < branches.length; i++) {
    args.push("-f", `b${i}=${branches[i]}`);
  }

  const r = await run(args, { cwd: config.paths.mainClone, timeoutMs: 15_000, signal });
  if (r.exitCode !== 0) return empty;
  let parsed: GqlResponse;
  try {
    parsed = JSON.parse(r.stdout);
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), {
      stdout: r.stdout.slice(0, 200),
      branchCount: branches.length,
    });
    return empty;
  }
  const repo = parsed.data?.repository;
  if (!repo) return empty;

  const prs = new Map<string, PullRequest>();
  for (let i = 0; i < branches.length; i++) {
    const nodes = repo[`wt_${i}`]?.nodes ?? [];
    if (nodes.length === 0) continue;
    // Prefer OPEN when a branch has multiple PRs (reopens etc.). Sort
    // is stable, so among non-OPEN we keep UPDATED_AT-desc order.
    const sorted = [...nodes].sort(
      (a, b) => (a.state === "OPEN" ? 0 : 1) - (b.state === "OPEN" ? 0 : 1),
    );
    const chosen = sorted[0];
    if (!chosen || !chosen.headRefName) continue;
    prs.set(chosen.headRefName, nodeToPr(chosen));
  }

  return { prs };
}

/**
 * Pull requests where the authenticated user (or one of their teams)
 * has been asked to review. Uses GitHub's `search` GraphQL — same auth
 * channel as `fetchGithub`, but a separate round trip because the
 * result set isn't keyed by worktree branches and the response shape is
 * narrower (no review threads, no requested reviewers list, no
 * suggestedReviewers). Capped at 50 since this is meant to be a
 * digestible "what's on your plate" list, not an inbox.
 */
const REVIEW_REQUESTS_QUERY = `
query {
  search(query: "is:pr is:open review-requested:@me", type: ISSUE, first: 50) {
    nodes {
      ... on PullRequest {
        number
        url
        title
        isDraft
        createdAt
        updatedAt
        author { login }
        repository { nameWithOwner }
        commits(last: 1) {
          nodes {
            commit {
              statusCheckRollup {
                contexts(first: 50) {
                  nodes {
                    __typename
                    ... on CheckRun { name status conclusion }
                    ... on StatusContext { context state }
                  }
                }
              }
            }
          }
        }
      }
    }
  }
}`;

type GqlReviewRequestNode = {
  number?: number;
  url?: string;
  title?: string;
  isDraft?: boolean;
  createdAt?: string;
  updatedAt?: string;
  author?: { login?: string | null } | null;
  repository?: { nameWithOwner?: string } | null;
  commits?: {
    nodes: Array<{
      commit: {
        statusCheckRollup: { contexts: { nodes: RawCheck[] } } | null;
      };
    }>;
  } | null;
};

type GqlReviewRequestResponse = {
  // `search(type: ISSUE)` returns a heterogeneous node list; for a node
  // that resolved to a non-`PullRequest` typename (or an empty object
  // from a deleted/inaccessible item) the spread fragment yields `{}`.
  // Modelled as nullable so the parser doesn't crash on those.
  data?: { search?: { nodes?: Array<GqlReviewRequestNode | null> } };
};

export async function fetchReviewRequests(
  signal?: AbortSignal,
): Promise<ReviewRequestPr[]> {
  if (!(await hasGh())) return [];
  const r = await run(
    ["gh", "api", "graphql", "-f", `query=${REVIEW_REQUESTS_QUERY}`],
    { cwd: config.paths.mainClone, timeoutMs: 15_000, signal },
  );
  if (r.exitCode !== 0) {
    log.error("review-requests fetch failed", { stderr: r.stderr.slice(0, 200) });
    return [];
  }
  let parsed: GqlReviewRequestResponse;
  try {
    parsed = JSON.parse(r.stdout);
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), {
      stdout: r.stdout.slice(0, 200),
    });
    return [];
  }
  const nodes = parsed.data?.search?.nodes ?? [];
  const out: ReviewRequestPr[] = [];
  for (const n of nodes) {
    // Drop incomplete nodes defensively — search returns `... on
    // PullRequest`-typed fragments, so a null or empty object means it
    // wasn't a PR (shouldn't happen with `is:pr` filter, but the type
    // is `[Issue | PullRequest | ...]`).
    if (!n) continue;
    if (typeof n.number !== "number" || !n.url || !n.title) continue;
    const contexts =
      n.commits?.nodes[0]?.commit?.statusCheckRollup?.contexts?.nodes ?? null;
    out.push({
      number: n.number,
      url: n.url,
      title: n.title,
      repoNameWithOwner: n.repository?.nameWithOwner ?? "",
      author: n.author?.login ?? null,
      isDraft: n.isDraft ?? false,
      checks: rollupChecks(contexts),
      createdAt: n.createdAt ?? "",
      updatedAt: n.updatedAt ?? "",
    });
  }
  return out;
}

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

/**
 * The currently-authenticated GitHub user's login. Cached for the
 * life of the process — gh auth doesn't change while the TUI is
 * running. Used to filter the user out of reviewer pickers (you
 * can't review your own PR).
 */
let _authedLogin: string | null | undefined;
export async function fetchAuthenticatedLogin(): Promise<string | null> {
  if (_authedLogin !== undefined) return _authedLogin;
  if (!(await hasGh())) {
    _authedLogin = null;
    return null;
  }
  const r = await run(["gh", "api", "user", "--jq", ".login"], {
    cwd: config.paths.mainClone,
    timeoutMs: 5_000,
  });
  if (r.exitCode !== 0) {
    log.error("auth user fetch failed", {
      stderr: r.stderr.slice(0, 200),
    });
    _authedLogin = null;
    return null;
  }
  const login = r.stdout.trim();
  _authedLogin = login.length > 0 ? login : null;
  return _authedLogin;
}

/**
 * Thin wrapper kept for CLI callers (doctor, ls) that don't have a
 * prebuilt branch list. Resolves branches from `git worktree list`
 * before fetching.
 */
export async function fetchPrs(): Promise<Map<string, PullRequest>> {
  const branches = (await listWorktrees())
    .filter((w) => !w.isMain && w.branch)
    .map((w) => w.branch as string);
  return (await fetchGithub(branches)).prs;
}

export type GhActionResult = { ok: true } | { ok: false; error: string };

/**
 * Edit a PR's review requests via `gh pr edit`. Both `add` and
 * `remove` may be passed in the same call — gh accepts both flag
 * sets at once. Logins are users; team slugs use the `org/team-slug`
 * form. Empty changes is a no-op.
 */
export async function editReviewers(
  prNumber: number,
  changes: { add: readonly string[]; remove: readonly string[] },
): Promise<GhActionResult> {
  if (changes.add.length === 0 && changes.remove.length === 0) {
    return { ok: true };
  }
  if (!(await hasGh())) return { ok: false, error: "gh CLI not found" };
  const argv = ["gh", "pr", "edit", String(prNumber)];
  for (const l of changes.add) argv.push("--add-reviewer", l);
  for (const l of changes.remove) argv.push("--remove-reviewer", l);
  const r = await run(argv, { cwd: config.paths.mainClone, timeoutMs: 15_000 });
  if (r.exitCode !== 0) {
    const msg = (r.stderr || r.stdout).trim() || `gh exited ${r.exitCode}`;
    log.error("edit reviewers failed", { prNumber, changes, msg });
    return { ok: false, error: msg };
  }
  return { ok: true };
}

/**
 * Flip a draft PR to "ready for review" via `gh pr ready`. Notifies
 * reviewers and triggers any code-owner auto-requests, so callers
 * should gate on user confirmation. Runs from the main clone so gh
 * resolves the right repo.
 */
export async function markPullRequestReady(
  prNumber: number,
): Promise<GhActionResult> {
  if (!(await hasGh())) return { ok: false, error: "gh CLI not found" };
  const r = await run(
    ["gh", "pr", "ready", String(prNumber)],
    { cwd: config.paths.mainClone, timeoutMs: 15_000 },
  );
  if (r.exitCode !== 0) {
    const msg = (r.stderr || r.stdout).trim() || `gh exited ${r.exitCode}`;
    log.error("mark ready failed", { prNumber, msg });
    return { ok: false, error: msg };
  }
  return { ok: true };
}

/**
 * Resolve the PR that belongs to this worktree's *current era*. A
 * worktree can be rm'd and then recreated on the same branch (e.g. an
 * issue is reopened); on the re-created era the old merged PR is
 * historical, not current. We detect that by comparing the PR's
 * terminal timestamp (mergedAt/closedAt) to the worktree directory's
 * birthtime: a terminal PR that finished before the directory was
 * created belongs to a previous era and is dropped.
 *
 * OPEN PRs are always kept — they have no terminal timestamp, and an
 * open PR on the branch is definitionally current regardless of when
 * it was filed.
 */
export function pickPrForWorktree(
  wt: Worktree,
  prs: Map<string, PullRequest> | Record<string, PullRequest> | undefined,
): PullRequest | undefined {
  if (!wt.branch || !prs) return undefined;
  const pr = prs instanceof Map ? prs.get(wt.branch) : prs[wt.branch];
  if (!pr) return undefined;
  if (pr.state === "OPEN") return pr;
  const terminalAt = pr.mergedAt ?? pr.closedAt;
  if (!terminalAt) return pr;
  let birthMs: number;
  try {
    const st = statSync(wt.path);
    birthMs = st.birthtimeMs || st.ctimeMs;
  } catch {
    // Path vanished (StatusKind.Missing) — no way to gate by birthtime.
    // Keep the PR so the user sees *something* attached to the row.
    return pr;
  }
  return Date.parse(terminalAt) < birthMs ? undefined : pr;
}
