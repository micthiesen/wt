import { config } from "../config.ts";
import { createLogger } from "../logger.ts";
import { run } from "../proc.ts";
import type { MergeQueueEntry, PullRequest } from "../types.ts";
import { listWorktrees } from "../worktree.ts";
import { hasGh, repoSlug } from "./gh-cli.ts";
import { nodeToPr } from "./parse.ts";
import type { GithubData, GqlResponse } from "./types.ts";

const log = createLogger("[gh]");

// Invariant: the github source is ONE batched GraphQL round trip. Every
// per-worktree PR field (state, checks, reviews, requested + suggested
// reviewers, ...) rides the same aliased query below, alongside the repo
// mergeQueue block. New PR fields go into PR_FRAGMENT here — never a new
// query; rate limits and latency are real.

// Shared fields for each PR. Used by every aliased sub-query below.
const PR_FRAGMENT = `
fragment PrFields on PullRequest {
  number
  url
  title
  headRefName
  headRefOid
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
  autoMergeRequest {
    enabledAt
    mergeMethod
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
        nodes { author { login __typename } }
      }
    }
  }
  comments(last: 10) {
    nodes {
      author { login __typename }
      body
      createdAt
    }
  }
  reviews(last: 10) {
    nodes {
      author { login __typename }
      body
      state
      createdAt
    }
  }
}`;

/**
 * Build a graphql doc with one aliased `pullRequests(headRefName:)`
 * sub-query per branch, plus the merge queue. `first: 2` per branch
 * catches the rare "branch has a reopen" case where there's an OPEN
 * and a terminal PR on the same ref — we'll prefer OPEN at parse time.
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
    // "no worktrees" case. No worktrees → nothing to show a merge-queue
    // position for, so the queue is skipped here too.
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
    mergeQueue {
      entries(first: 50) {
        nodes {
          enqueuedAt
          estimatedTimeToMerge
          position
          state
          pullRequest { headRefName }
        }
      }
    }
  }
}
${PR_FRAGMENT}`;
}

/**
 * Fetch PRs for a fixed set of branches + merge-queue entries in a
 * single graphql round trip. Returns empty maps on any failure — the
 * TUI treats "no entry" as "not there" rather than surfacing transient
 * errors. Pass the exact worktree branches; anything not on the list
 * is never fetched (the TUI wouldn't display it anyway).
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
  const empty: GithubData = { prs: new Map(), mergeQueue: new Map() };
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

  const mergeQueue = new Map<string, MergeQueueEntry>();
  for (const n of repo.mergeQueue?.entries?.nodes ?? []) {
    const head = n.pullRequest?.headRefName;
    if (!head) continue;
    mergeQueue.set(head, {
      headRefName: head,
      position: n.position,
      state: n.state,
      enqueuedAt: n.enqueuedAt,
      estimatedTimeToMerge: n.estimatedTimeToMerge,
    });
  }

  return { prs, mergeQueue };
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
