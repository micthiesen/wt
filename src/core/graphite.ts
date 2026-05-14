import { config } from "./config.ts";
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

/**
 * The URL the `p` / `⏎` keybind (and the yank menu's `r`) should open
 * for a GitHub PR, honoring the `github.pr_viewer` setting. `"graphite"`
 * rewrites to the Graphite reskin; `"github"` (the default) passes the
 * github.com URL through. Falls back to the GitHub URL if the Graphite
 * rewrite can't parse the input.
 */
export function prViewerUrl(githubPrUrl: string): string {
  if (config.github.prViewer === "graphite") {
    return graphiteUrlFromGithubPr(githubPrUrl) ?? githubPrUrl;
  }
  return githubPrUrl;
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
 * Runs `gt track --parent` first so the branch is known to Graphite.
 * Untracked branches fail `gt submit` with "Cannot perform this
 * operation on untracked branch"; `gt track` is idempotent so we
 * always run it. Caller passes the parent branch — typically derived
 * from `row.stackedOn?.branch` falling back to the configured trunk.
 *
 * There is no documented disarm path; Graphite expects you to flip
 * the "Merge when ready" toggle off in `app.graphite.com`.
 */
export async function armMergeWhenReady(
  wtPath: string,
  parent: string,
): Promise<GraphiteActionResult> {
  const track = await run(
    ["gt", "track", "--parent", parent, "--no-interactive"],
    { cwd: wtPath, timeoutMs: 10_000 },
  );
  if (track.exitCode !== 0) {
    const msg = (track.stderr || track.stdout).trim() || `gt track exited ${track.exitCode}`;
    log.error("gt track failed", { wtPath, parent, msg });
    return { ok: false, error: msg };
  }
  const arm = await run(["gt", "submit", "--merge-when-ready"], {
    cwd: wtPath,
    timeoutMs: 60_000,
  });
  if (arm.exitCode !== 0) {
    const msg = (arm.stderr || arm.stdout).trim() || `gt exited ${arm.exitCode}`;
    log.error("gt submit --merge-when-ready failed", { wtPath, msg });
    return { ok: false, error: msg };
  }
  return { ok: true };
}
