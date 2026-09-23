/**
 * `wt merge` — arm GitHub's "Merge when ready" on a worktree's PR, the
 * same thing the TUI's `m` picker row does and the same thing the
 * button on the PR page does.
 *
 * This exists because agents had no way to reach it. The obvious
 * substitute, `gh pr merge --auto`, implements only ONE of the two
 * GitHub features that wear the "merge when ready" label: it calls
 * `enablePullRequestAutoMerge`, which a repo with `allow_auto_merge:
 * false` rejects outright — so on a repo whose base branch has a merge
 * QUEUE, the button works in the browser and the CLI reports `Auto
 * merge is not allowed for this repository`. That error names a repo
 * setting which has nothing to do with the failure, so the agent reads
 * it as a permissions problem and escalates to the human, who can only
 * click the button that was always going to work. `enableAutoMerge`
 * picks the right mutation off the PR's base branch; all this command
 * does is put a CLI in front of it.
 */
import {
  disableAutoMerge,
  enableAutoMerge,
  viewPrInfo,
} from "../../core/github.ts";
import { listWorktrees, type WorktreeError } from "../../core/worktree.ts";
import { agentIdentity } from "../../core/agent-identity.ts";
import { hasHelpFlag } from "../args.ts";
import { dim, green, red, yellow } from "../colors.ts";
import { Effect } from "effect";

const USAGE = `usage: wt merge [<slug>]            arm "merge when ready" on the PR
       wt merge --cancel [<slug>]   cancel it

Arms the PR to merge itself once its requirements are met — the same
action as the "Merge when ready" button on the PR page. Where the base
branch has a merge queue the PR is ENQUEUED; otherwise classic
auto-merge is armed. Those are two different GitHub features gated on
different settings, which is why \`gh pr merge --auto\` is not a
substitute: it only ever does the second, and fails on a queue repo
with an error naming a repo setting that is not the reason.

Never merges on the spot: if the PR is not ready, GitHub holds it.
--cancel checks the PR's actual state: dequeues an entry or disarms
classic auto-merge, even when that PR's base has a merge queue.

<slug> defaults to $WT_AGENT, then to the worktree containing the
current directory. Exits 75 when the refusal is temporary (a required
check has not reported yet), so a caller can retry rather than
escalate.`;

/** Resolve the target worktree: explicit arg → $WT_AGENT → cwd. */
const resolveTarget = Effect.fn("wt merge resolveTarget")(function* (
  slugOrBranch: string | undefined,
): Effect.fn.Return<{ slug: string; branch: string } | { error: string }, WorktreeError> {
  const wts = (yield* listWorktrees()).filter((w) => !w.isMain);
  if (slugOrBranch) {
    const wt = wts.find(
      (w) => w.slug === slugOrBranch || w.branch === slugOrBranch,
    );
    return wt
      ? { slug: wt.slug, branch: wt.branch }
      : { error: `no worktree: ${slugOrBranch}` };
  }
  const agent = agentIdentity();
  if (agent) {
    const wt = wts.find((w) => w.slug === agent);
    if (wt) return { slug: wt.slug, branch: wt.branch };
  }
  // Longest path first: a slug is routinely a strict prefix of another
  // slug, so the shortest containing path can be a neighbour's.
  const cwd = process.cwd();
  const byPath = wts
    .filter((w) => cwd === w.path || cwd.startsWith(`${w.path}/`))
    .sort((a, b) => b.path.length - a.path.length)[0];
  if (byPath) return { slug: byPath.slug, branch: byPath.branch };
  return { error: "no worktree given, and this directory isn't in one" };
});

export const run = Effect.fn("wt merge")(function* (argv: string[]) {
  if (hasHelpFlag(argv)) {
    console.log(USAGE);
    return 0;
  }
  const cancel = argv.includes("--cancel");
  const positional = argv.filter((a) => !a.startsWith("-"));
  if (positional.length > 1) {
    console.error(red("expected at most one <slug>"));
    return 2;
  }
  const unknown = argv.find((a) => a.startsWith("-") && a !== "--cancel");
  if (unknown) {
    console.error(red(`unknown flag: ${unknown}`));
    console.error(dim(USAGE));
    return 2;
  }

  const target = yield* resolveTarget(positional[0]);
  if ("error" in target) {
    console.error(red(target.error));
    return 1;
  }

  const pr = yield* viewPrInfo(target.branch);
  if (!pr) {
    console.error(red(`no PR for ${target.branch}`));
    console.error(dim("  (or gh is unavailable / not authenticated)"));
    return 1;
  }
  if (pr.state !== "OPEN") {
    console.error(red(`#${pr.number} is ${pr.state.toLowerCase()}`));
    return 1;
  }

  if (cancel) {
    const res = yield* disableAutoMerge(pr.number, {
      prId: pr.id,
      baseRefName: pr.baseRefName,
    });
    if (!res.ok) {
      console.error(red(res.error));
      return 1;
    }
    console.log(green(`cancelled merge-when-ready on #${pr.number}`));
    return 0;
  }

  // A draft is refused HERE rather than by GitHub, because GitHub's own
  // refusal for this case is unhelpful and the remedy is one command.
  if (pr.isDraft) {
    console.error(red(`#${pr.number} is a draft`));
    console.error(dim(`  mark it ready first: gh pr ready ${pr.number}`));
    return 1;
  }
  if (!pr.id) {
    console.error(red(`could not read #${pr.number}'s node id from gh`));
    return 1;
  }

  const res = yield* enableAutoMerge(pr.id, {
    baseRefName: pr.baseRefName,
    headRefOid: pr.headRefOid,
  });
  if (res.ok) {
    console.log(
      green(`merge when ready armed on #${pr.number}`) +
        dim(` → ${pr.baseRefName}`),
    );
    console.log(dim("  GitHub merges it once its requirements are met."));
    return 0;
  }
  console.error(red(res.error));
  if (res.retryable) {
    // Exit 75 (EX_TEMPFAIL), same contract as a full dev-slot queue: the
    // refusal is a clock, not a verdict, so the caller should wait and
    // retry rather than report a blocker to a human.
    console.error(yellow("  temporary — retry once the check reports."));
    return 75;
  }
  return 1;
});
