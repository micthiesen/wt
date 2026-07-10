import { config } from "../config.ts";
import { createLogger } from "../logger.ts";
import { run } from "../proc.ts";
import { hasGh } from "./gh-cli.ts";
import { openPrChecks, rollupChecks } from "./parse.ts";
import type { RawCheck } from "./types.ts";
import type { ReviewRequestPr } from "./types.ts";

const log = createLogger("[gh]");

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
        headRefName
        additions
        deletions
        changedFiles
        reviewDecision
        comments { totalCount }
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
  headRefName?: string | null;
  additions?: number;
  deletions?: number;
  changedFiles?: number;
  reviewDecision?: string | null;
  comments?: { totalCount?: number } | null;
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
    // An aborted signal means the query was cancelled mid-flight (refs
    // churn invalidates this query; `run` SIGTERMs the child → exit
    // 143). TanStack discards the cancelled fetch's result, so this is
    // routine supersession, not a failure — stay silent.
    if (signal?.aborted) return [];
    // `gh api graphql` puts GraphQL errors / rate-limit bodies on
    // stdout, not stderr — and a timeout/abort leaves both empty with
    // only a non-zero exit code. Log all three so the failure is
    // actually diagnosable instead of `{"stderr":""}`.
    log.error("review-requests fetch failed", {
      exitCode: r.exitCode,
      stderr: r.stderr.slice(0, 200) || null,
      stdout: r.stdout.slice(0, 200) || null,
    });
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
    const decision =
      n.reviewDecision === "APPROVED" ||
      n.reviewDecision === "CHANGES_REQUESTED" ||
      n.reviewDecision === "REVIEW_REQUIRED"
        ? n.reviewDecision
        : null;
    out.push({
      number: n.number,
      url: n.url,
      title: n.title,
      repoNameWithOwner: n.repository?.nameWithOwner ?? "",
      headRefName: n.headRefName ?? null,
      author: n.author?.login ?? null,
      isDraft: n.isDraft ?? false,
      // The search is `is:open`, so every row here is an open PR — floor an
      // empty rollup at `pending` the same way the worktree rows do.
      checks: openPrChecks("OPEN", rollupChecks(contexts)),
      reviewDecision: decision,
      additions: n.additions ?? 0,
      deletions: n.deletions ?? 0,
      changedFiles: n.changedFiles ?? 0,
      commentCount: n.comments?.totalCount ?? 0,
      createdAt: n.createdAt ?? "",
      updatedAt: n.updatedAt ?? "",
    });
  }
  return out;
}
