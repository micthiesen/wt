import type { PullRequest } from "../core/types.ts";

export type Landing = "base" | "production" | null;

/** Never promote a row from branch location alone: it must have landed. */
export function resolveLanding(
  mergedIntoBase: boolean | undefined,
  pr: Pick<PullRequest, "state" | "baseRefName" | "mergeCommitOid"> | undefined,
  base: string,
  production: string | null,
  productionCommits: readonly string[] | undefined,
): Landing {
  const onBase = mergedIntoBase === true ||
    (pr?.state === "MERGED" && pr.baseRefName === base);
  if (!onBase) return null;
  if (production === base) return "production";
  if (production && pr?.mergeCommitOid && productionCommits?.includes(pr.mergeCommitOid)) {
    return "production";
  }
  return "base";
}
