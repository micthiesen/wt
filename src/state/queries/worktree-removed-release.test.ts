import { expect, test } from "bun:test";

import { parseRemovedPrMerges } from "./worktree.ts";

const sha = (digit: string) => digit.repeat(40);

test("legacy removed PRs use only exact first-parent merge subjects", () => {
  const log = [
    `${sha("a")}\0Merge pull request #2162 from owner/branch`,
    `${sha("b")}\0Merge pull request #2161 from owner/other`,
    `${sha("c")}\0Mention #2160 in another commit`,
    `${sha("d")}\0Merge pull request #21620 from owner/not-2162`,
    `${sha("e")}\0Merge pull request #2162 from owner/older`,
  ].join("\n");
  expect(parseRemovedPrMerges(log, [2162, 2160, 9999])).toEqual({ 2162: sha("a") });
});
