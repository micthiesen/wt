import { git } from "../../../core/git.ts";
import { addSliceToStack } from "../../../core/stack-ops.ts";
import { findStackIdByBranch } from "../../../core/wtstate.ts";
import { bold, dim, green, red } from "../../colors.ts";
import { logLine, stackIdFromCwd } from "./shared.ts";

/**
 * `wt stack add [<branch>] [<stackId>] [--onto <sliceId|branch>] [--title <t>]`
 *
 * Positional disambiguation: branches in this workflow always carry a
 * namespace (`michael/…`), stackIds never do — so a positional with a `/` is
 * the branch, without is the stackId. The branch defaults to the current
 * worktree's HEAD (the common flow: `wt new --base <tip>`, work, then run
 * `add` from inside the new worktree). The stackId resolves from the cwd
 * branch when it's already a tracked slice, else from `--onto` when that
 * names a tracked branch — the new branch itself is in no manifest yet, so
 * from its own worktree you pass the stack via `--onto` or positionally.
 */
export async function runAdd(argv: string[]): Promise<number> {
  let branch: string | undefined;
  let stackId: string | undefined;
  let onto: string | undefined;
  let title: string | undefined;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === "--onto") {
      onto = argv[++i];
      if (!onto) {
        console.error(red("--onto requires a slice id or branch"));
        return 2;
      }
    } else if (a === "--title") {
      title = argv[++i];
      if (!title) {
        console.error(red("--title requires a value"));
        return 2;
      }
    } else if (a.startsWith("--")) {
      console.error(red(`unknown flag: ${a}`));
      return 2;
    } else if (a.includes("/")) {
      if (branch) {
        console.error(red(`two branch args: ${branch}, ${a}`));
        return 2;
      }
      branch = a;
    } else if (!stackId) stackId = a;
    else {
      console.error(red(`unexpected arg: ${a}`));
      return 2;
    }
  }
  if (!branch) {
    try {
      const head = (await git(["rev-parse", "--abbrev-ref", "HEAD"], process.cwd())).trim();
      if (head && head !== "HEAD") branch = head;
    } catch {
      // fall through to the usage error
    }
  }
  if (!branch) {
    console.error(red("no branch given and the current directory has no checked-out branch"));
    return 2;
  }
  if (!stackId) stackId = (await stackIdFromCwd()) ?? undefined;
  if (!stackId && onto?.includes("/")) stackId = findStackIdByBranch(onto) ?? undefined;
  if (!stackId) {
    console.error(
      red(
        "usage: wt stack add [<branch>] [<stackId>] [--onto <sliceId|branch>] [--title <t>]\n" +
          "  (couldn't resolve the target stack — pass <stackId>, or --onto a tracked branch)",
      ),
    );
    return 2;
  }

  const res = await addSliceToStack(stackId, branch, { onto, title }, logLine);
  if (!res.ok) {
    console.error(red(res.error));
    return 1;
  }
  const s = res.slice;
  console.log(
    green(
      `✓ added ${bold(s.id)} to ${bold(stackId)} — ${s.branch} onto ${res.parentBranch} (PR #${s.pr} ${res.prAction})`,
    ),
  );
  console.log(
    `  ${dim(String(s.ordinal).padStart(2, "0"))} ${s.title}  ${dim(`anchor ${s.baseSha?.slice(0, 9) ?? "?"}, ${s.files.length} file(s)${s.partials?.length ? ` + ${s.partials.length} partial` : ""}`)}`,
  );
  // Same staleness as a re-split, milder: existing PR bodies' stack sections
  // now list a slice set missing the new tip. wt never authors bodies.
  console.log(dim("  note: sibling PR stack-sections don't list the new slice — regenerate if you care"));
  return 0;
}
