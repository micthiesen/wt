import { statSync } from "node:fs";

import { config } from "./config.ts";
import { createLogger } from "./logger.ts";
import { run } from "./proc.ts";
import type {
  MergeQueueEntry,
  MergeQueueState,
  PrChecks,
  PrReview,
  PullRequest,
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
async function repoSlug(): Promise<string | null> {
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
  | { __typename: "CheckRun"; status?: string | null; conclusion?: string | null }
  | { __typename: "StatusContext"; state?: string | null };

const CHECK_FAIL_CONCLUSIONS = new Set([
  "FAILURE",
  "CANCELLED",
  "TIMED_OUT",
  "ACTION_REQUIRED",
  "STARTUP_FAILURE",
]);

function rollupChecks(raw: RawCheck[] | null | undefined): PrChecks {
  if (!raw || raw.length === 0) return "none";
  let pending = false;
  let fail = false;
  for (const c of raw) {
    if (c.__typename === "CheckRun") {
      if (c.status && c.status !== "COMPLETED") pending = true;
      else if (c.conclusion && CHECK_FAIL_CONCLUSIONS.has(c.conclusion)) fail = true;
    } else {
      const s = c.state;
      if (s === "PENDING" || s === "EXPECTED") pending = true;
      else if (s === "FAILURE" || s === "ERROR") fail = true;
    }
  }
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
  isDraft
  state
  mergedAt
  closedAt
  reviewDecision
  reviewRequests(first: 1) { totalCount }
  commits(last: 1) {
    nodes {
      commit {
        statusCheckRollup {
          contexts(first: 50) {
            nodes {
              __typename
              ... on CheckRun { status conclusion }
              ... on StatusContext { state }
            }
          }
        }
      }
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
  const varDecls = Array.from({ length: branchCount }, (_, i) => `$b${i}: String!`).join(", ");
  const varsClause = varDecls ? `, ${varDecls}` : "";
  const aliases = Array.from({ length: branchCount }, (_, i) =>
    `    wt_${i}: pullRequests(first: 2, headRefName: $b${i}, orderBy: {field: UPDATED_AT, direction: DESC}) { nodes { ...PrFields } }`,
  ).join("\n");
  // Omit the fragment when there are no branches — graphql rejects
  // unused fragments as a validation error.
  const fragment = branchCount > 0 ? PR_FRAGMENT : "";
  return `
query($owner: String!, $name: String!${varsClause}) {
  repository(owner: $owner, name: $name) {
${aliases ? aliases + "\n" : ""}    mergeQueue {
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
${fragment}`;
}

type GqlReviewDecision = "APPROVED" | "CHANGES_REQUESTED" | "REVIEW_REQUIRED" | null;

type GqlPrNode = {
  number: number;
  url: string;
  title: string;
  headRefName: string;
  isDraft: boolean;
  state: PullRequest["state"];
  mergedAt: string | null;
  closedAt: string | null;
  reviewDecision: GqlReviewDecision;
  reviewRequests: { totalCount: number } | null;
  commits: {
    nodes: Array<{
      commit: {
        statusCheckRollup: { contexts: { nodes: RawCheck[] } } | null;
      };
    }>;
  };
};

function rollupReview(state: PullRequest["state"], decision: GqlReviewDecision): PrReview {
  // Terminal PRs don't carry useful review state; suppress to keep the
  // line uncluttered. The PR badge already conveys merged/closed.
  if (state !== "OPEN") return "none";
  if (decision === "APPROVED") return "approved";
  if (decision === "CHANGES_REQUESTED") return "changes_requested";
  return "pending";
}

type GqlMqEntry = {
  enqueuedAt: string;
  estimatedTimeToMerge: number | null;
  position: number;
  state: MergeQueueState;
  pullRequest: { headRefName: string } | null;
};

type GqlRepo = {
  mergeQueue?: { entries?: { nodes?: GqlMqEntry[] } } | null;
  // Each aliased `wt_N` key lands here as `{ nodes: GqlPrNode[] }`.
  [alias: `wt_${number}`]: { nodes?: GqlPrNode[] } | undefined;
};

type GqlResponse = {
  data?: { repository?: GqlRepo };
};

export type GithubData = {
  prs: Map<string, PullRequest>;
  mergeQueue: Map<string, MergeQueueEntry>;
};

function nodeToPr(pr: GqlPrNode): PullRequest {
  const contexts =
    pr.commits.nodes[0]?.commit?.statusCheckRollup?.contexts?.nodes ?? null;
  return {
    number: pr.number,
    url: pr.url,
    title: pr.title,
    headRefName: pr.headRefName,
    isDraft: pr.isDraft,
    state: pr.state,
    checks: rollupChecks(contexts),
    review: rollupReview(pr.state, pr.reviewDecision),
    reviewRequests: pr.reviewRequests?.totalCount ?? 0,
    mergedAt: pr.mergedAt ?? null,
    closedAt: pr.closedAt ?? null,
  };
}

/**
 * Fetch PRs for a fixed set of branches + merge-queue entries in a
 * single graphql round trip. Returns empty maps on any failure — the
 * TUI treats "no entry" as "not there" rather than surfacing transient
 * errors. Pass the exact worktree branches; anything not on the list
 * is never fetched (the TUI wouldn't display it anyway).
 */
export async function fetchGithub(branches: string[]): Promise<GithubData> {
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

  const r = await run(args, { cwd: config.paths.mainClone, timeoutMs: 15_000 });
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
