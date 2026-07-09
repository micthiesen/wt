import { basename } from "node:path";

import { config } from "../../../core/config.ts";
import { gitRun } from "../../../core/git.ts";
import { readWtState } from "../../../core/wtstate.ts";
import { fetchOrigin } from "../../../core/worktree.ts";
import { run as sizeRun } from "../size.ts";

/**
 * Read-only pre-split context, printed for the /split skill to consume.
 * Folds the old `split/scripts/context.sh`: fetch origin (so trunk isn't
 * stale right after a parent merge), then report branch / cleanliness /
 * the base decision / changed-file inventory / `wt size`. Runs against the
 * CURRENT directory (the holistic worktree), not the main clone.
 *
 * The base decision mirrors the recorded-fork-base precedence: a freshly
 * rebased parent chain puts HEAD on top of trunk too, so the bare
 * is-ancestor check would fold an unmerged parent's work into the slices —
 * consult `wt base` (the `wt new --base` record) first.
 */
export async function runContext(_argv: string[]): Promise<number> {
  const cwd = process.cwd();
  // /split often runs right after merging a parent PR, when origin/<trunk>
  // is stale; a stale base folds already-merged work into the slices. Surface
  // a failed fetch so the base decision below isn't silently trusted offline.
  try {
    await fetchOrigin();
  } catch {
    console.log("(git fetch failed — base may be stale)");
  }

  const branchR = await gitRun(["rev-parse", "--abbrev-ref", "HEAD"], cwd);
  const branch = branchR.exitCode === 0 ? branchR.stdout.trim() : "?";
  let base = `origin/${config.branch.base}`;

  const statusR = await gitRun(["status", "--porcelain"], cwd);
  const clean = statusR.exitCode === 0 && statusR.stdout.trim() === "";
  console.log(`branch:     ${branch}`);
  console.log(`tree clean: ${clean ? "yes" : "NO"}`);

  const topR = await gitRun(["rev-parse", "--show-toplevel"], cwd);
  const slug = topR.exitCode === 0 ? basename(topR.stdout.trim()) : "";
  const rec = (slug && readWtState().slugs[slug]?.baseBranch) || "";
  const isAncestor = async (ref: string): Promise<boolean> =>
    ref.length > 0 &&
    (await gitRun(["merge-base", "--is-ancestor", ref, "HEAD"], cwd)).exitCode === 0;

  if (rec && (await isAncestor(rec))) {
    console.log(`recorded fork base: ${rec} — CONFIRMED (HEAD is on top of it).`);
    console.log("  -> stacked on this unmerged parent (case b). Use it as the base for the");
    console.log("     diff and every root slice; no need to ask.");
    base = rec;
  } else if (await isAncestor(base)) {
    console.log(`base status: OK — HEAD is on top of current ${base}`);
  } else {
    console.log(`base status: HEAD is NOT on current ${base}`);
    if (rec) console.log(`  recorded fork base: ${rec} — but HEAD is NOT on top of it (drifted?).`);
    console.log("  Do NOT assume. Either:");
    console.log(`  (a) main moved / a parent PR merged -> 'git rebase ${base}' (base = main)`);
    console.log("  (b) stacked on an unmerged parent    -> base = that parent branch");
    console.log("  The skill asks which before slicing (never silently rebase or split stale).");
  }
  console.log(`base:       ${base}`);

  const mb = await gitRun(["merge-base", base, "HEAD"], cwd);
  console.log(`merge-base: ${mb.exitCode === 0 ? mb.stdout.trim() : "?"}`);
  console.log("");
  console.log(`changed files (vs ${base}):`);
  const diff = await gitRun(["diff", "--stat", `${base}...HEAD`], cwd);
  if (diff.exitCode === 0 && diff.stdout.trim() !== "") {
    // mirror the old script's `tail -60`
    console.log(diff.stdout.replace(/\n+$/, "").split("\n").slice(-60).join("\n"));
  }
  console.log("");
  console.log("wt size:");
  // Size against the SAME resolved base as the inventory above — otherwise a
  // slice stacked on an unmerged parent would be measured vs trunk and fold the
  // parent's already-stacked work into the budget read.
  await sizeRun(["--base", base, "--json"]);
  return 0;
}
