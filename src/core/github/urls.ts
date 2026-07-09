import { config, type PullRequestTarget } from "../config.ts";

/** Browser target for a PR URL, honoring `[github].pr_target`. */
export function pullRequestOpenUrl(githubUrl: string): string {
  return pullRequestOpenUrlForTarget(githubUrl, config.github.prTarget);
}

/** Browser target for a PR URL, ignoring config and using an explicit target. */
export function pullRequestOpenUrlForTarget(
  githubUrl: string,
  target: PullRequestTarget,
): string {
  if (target !== "linear") return githubUrl;
  return linearReviewUrl(githubUrl) ?? githubUrl;
}

/**
 * Linear Reviews can open an existing GitHub PR by rewriting the URL
 * to the `linear://` deep-link scheme, preserving `/owner/repo/pull/123`.
 * Mirrors the `https://linear.review/<owner>/<repo>/pull/<N>` redirect host.
 */
function linearReviewUrl(githubUrl: string): string | null {
  try {
    const url = new URL(githubUrl);
    if (url.hostname !== "github.com") return null;
    const parts = url.pathname.split("/").filter(Boolean);
    if (parts.length < 4 || parts[2] !== "pull") return null;
    return `linear://review/${parts[0]}/${parts[1]}/pull/${parts[3]}`;
  } catch {
    return null;
  }
}
