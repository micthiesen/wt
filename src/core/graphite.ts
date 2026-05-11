import { createLogger } from "./logger.ts";
import { run } from "./proc.ts";

const log = createLogger("[graphite]");

/**
 * Graphite review URL for a GitHub PR. Graphite uses GitHub as the
 * source of truth and reskins the PR view at a deterministic URL, so
 * we just rewrite the github.com URL we already have rather than
 * introducing a separate fetch.
 *
 * Returns null on a malformed input (defensive — `pr.url` from
 * `gh`'s GraphQL is always well-formed in practice).
 */
const GH_PR_URL_RE = /^https:\/\/github\.com\/([^/]+)\/([^/]+)\/pull\/(\d+)/;

export function graphiteUrlFromGithubPr(githubPrUrl: string): string | null {
  const m = GH_PR_URL_RE.exec(githubPrUrl);
  if (!m) return null;
  const [, owner, repo, number] = m;
  return `https://app.graphite.com/github/pr/${owner}/${repo}/${number}`;
}

export type GraphiteActionResult = { ok: true } | { ok: false; error: string };

/**
 * Arm Graphite's "merge when ready" on the current branch via
 * `gt submit --merge-when-ready`. We shell out rather than calling the
 * `/submit/pull-requests` API directly: the submit endpoint takes a
 * stack-aware payload (baseSha + headSha per PR, action discriminator)
 * that `gt` synthesizes from the local repo state — re-implementing
 * that would mean shadowing `gt`'s internal stack engine.
 *
 * Runs in `wtPath` so `gt` resolves the right repo and branch. There
 * is no documented disarm path; Graphite expects you to flip the
 * "Merge when ready" toggle off in `app.graphite.com` or remove the
 * configured merge-queue label on the PR.
 */
export async function armMergeWhenReady(
  wtPath: string,
): Promise<GraphiteActionResult> {
  const r = await run(["gt", "submit", "--merge-when-ready"], {
    cwd: wtPath,
    timeoutMs: 60_000,
  });
  if (r.exitCode !== 0) {
    const msg = (r.stderr || r.stdout).trim() || `gt exited ${r.exitCode}`;
    log.error("gt submit --merge-when-ready failed", { wtPath, msg });
    return { ok: false, error: msg };
  }
  return { ok: true };
}
