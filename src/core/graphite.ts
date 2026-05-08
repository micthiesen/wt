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
  return `https://app.graphite.dev/github/pr/${owner}/${repo}/${number}`;
}
