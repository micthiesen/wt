import { applyStack, replayStack, splitStack } from "../../../core/stack-ops.ts";
import { bold, cyan, dim, green, red, yellow } from "../../colors.ts";
import { readFragment } from "./manifest-io.ts";
import { logLine, reportReplayResult } from "./shared.ts";

export async function runSplit(argv: string[]): Promise<number> {
  let stackId: string | undefined;
  let sliceId: string | undefined;
  let from: string | undefined;
  let plan = false;
  let apply = false;
  let verify = false;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === "--plan") plan = true;
    else if (a === "--apply") apply = true;
    else if (a === "--verify") verify = true;
    else if (a === "--from") {
      from = argv[++i];
      if (!from) {
        console.error(red("--from requires a path"));
        return 2;
      }
    } else if (a.startsWith("--")) {
      console.error(red(`unknown flag: ${a}`));
      return 2;
    } else if (!stackId) stackId = a;
    else if (!sliceId) sliceId = a;
    else {
      console.error(red(`unexpected arg: ${a}`));
      return 2;
    }
  }
  if (plan && apply) {
    console.error(red("--plan and --apply are mutually exclusive"));
    return 2;
  }
  if (verify && !apply) {
    console.error(red("--verify only applies with --apply (it gates the chained apply)"));
    return 2;
  }
  if (!stackId || !sliceId || !from) {
    console.error(red("usage: wt stack split <stackId> <sliceId> --from <fragment.json> [--plan]"));
    return 2;
  }
  const fragment = readFragment(from);
  if (!fragment) return 1;
  const res = splitStack(stackId, sliceId, fragment, { plan });
  if (!res.ok) {
    console.error(red(res.error));
    return 1;
  }
  console.log(
    green(`${plan ? "would reshape" : "✓ reshaped"} ${bold(stackId)}: ${bold(sliceId)} → ${res.newSliceIds.join(", ")}`),
  );
  for (const s of res.slices) {
    const mark = res.newSliceIds.includes(s.id) ? cyan(" «new»") : "";
    console.log(`  ${dim(String(s.ordinal).padStart(2, "0"))} ${s.id}  ${dim("base:")} ${s.base}${mark}`);
  }
  console.log(dim(`\nsource for new slices: ${res.sourceBranch}`));
  if (res.rethreadedChildren.length > 0) {
    console.log(dim(`re-threaded onto new tip: ${res.rethreadedChildren.join(", ")}`));
  }
  if (plan) {
    console.log(dim("(--plan: nothing written)"));
    return 0;
  }

  const retire = res.supersededPr
    ? `gh pr close ${res.supersededPr} --delete-branch --comment "superseded by re-split"`
    : `git push origin --delete ${res.supersededBranch}`;

  // The split changed the slice SET, so every re-threaded descendant's PR body
  // still lists the OLD set (the now-superseded PR, none of the new sub-slices).
  // wt never authors PR bodies (that's `/split`'s job), so flag which bodies
  // are now stale rather than silently leaving them wrong.
  const staleBodies = [...res.newSliceIds.map((id) => `${id} (new)`), ...res.rethreadedChildren];
  if (staleBodies.length > 0) {
    console.log(
      yellow(`\n⚠ stale PR stack-sections — regenerate the bodies for: ${staleBodies.join(", ")}`),
    );
    console.log(dim("  (the slice set changed; descendant + new-slice stack sections list the old set)"));
  }

  if (!apply) {
    console.log(`\n${bold("next:")}`);
    console.log(`  1. wt stack apply ${stackId}     ${dim(`# materialize new sub-slices from ${res.sourceBranch}`)}`);
    console.log(`  2. wt stack replay ${stackId}    ${dim("# rebase + retarget the re-threaded descendants")}`);
    console.log(`  3. ${retire}  ${dim("# retire the split slice (after step 1)")}`);
    return 0;
  }

  // --apply: chain the mechanical happy-path (reshape → materialize → replay).
  // PR retirement stays an explicit printed step — it closes a PR + deletes a
  // branch, which wt won't do behind the user's back. With --verify the
  // chained apply typechecks each new sub-slice prefix BEFORE any PR opens —
  // the root-cause gate for a re-split whose ordering breaks compilation
  // (a sub-slice ordered ahead of the exhaustive consumer that needs it).
  console.log(
    `\n${bold("apply:")} materializing new sub-slices from ${res.sourceBranch}${verify ? " (verify)" : ""}`,
  );
  const applied = await applyStack(stackId, { install: false, verify }, logLine);
  if (applied.error) {
    console.error(red(applied.error));
    if (applied.materialized.length > 0) {
      console.error(dim(`(materialized before failure: ${applied.materialized.join(", ")})`));
    }
    return 1;
  }
  console.log(
    green(`✓ applied ${bold(stackId)} · ${applied.materialized.length} materialized, ${applied.skipped.length} skipped`),
  );

  console.log(`\n${bold("replay:")} rebasing re-threaded descendants onto the new tip`);
  const replayed = await replayStack(stackId, {}, logLine);
  const rc = reportReplayResult(stackId, "replayed", replayed);
  if (rc !== 0) return rc;

  console.log(`\n${bold("retire:")} ${retire}  ${dim("# close the superseded PR when ready")}`);
  return 0;
}
