import { git } from "../../core/git.ts";
import {
  pruneStackBackups,
  rebaseStack,
  type RebaseResult,
} from "../../core/stack-ops.ts";
import { bold, dim, green, red, yellow } from "../colors.ts";

const USAGE = `usage: wt restack [<branch>] [--onto <ref>]
       wt restack prune-backups [--days <n>]

Rebase a worktree — or the whole stack containing it — onto its
updated parents: reconcile each member's recorded fork base against
landed PRs (a merged parent reparents its children), then
squash-safe-replay every member onto its parent, force-push (branches
with no origin counterpart are rebased but not pushed), and retarget PR
bases. Stacks are inferred from the fork-base records (\`wt new
--base\` / \`wt base\`); any member's branch selects the whole stack,
and a standalone worktree is a one-member chain rebasing onto its
recorded base or plain trunk. With no <branch>, the current worktree's
branch is used.

Conflicts are never auto-resolved: the run bails naming the failing
branch and a backup ref — resolve in that worktree, then re-run (or use
/restack). Exit 3 on a conflict bail.

prune-backups deletes the engine's \`backup/restack-*\` refs, keeping
ones newer than --days (default 0 = delete all).`;

function logLine(line: string): void {
  console.log(dim(line));
}

/** Render a restack result; returns the process exit code. */
function report(target: string, result: RebaseResult): number {
  if (!result.ok) {
    console.error(red(result.error));
    if (result.conflict) {
      // Hand off to the resolving skill — wt never auto-resolves conflicts.
      if (result.failedBranch) console.error(yellow(`  failing branch: ${result.failedBranch}`));
      if (result.backupBranch) console.error(yellow(`  backup branch:  ${result.backupBranch}`));
      console.error(dim("  resolve in that worktree, then re-run `wt restack` (or /restack)."));
      return 3;
    }
    return 1;
  }
  if (result.output) console.log(dim(result.output));
  console.log(green(`✓ restacked ${bold(target)}`));
  return 0;
}

async function runPruneBackups(argv: string[]): Promise<number> {
  let days = 0;
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--days") {
      const n = Number(argv[++i]);
      if (!Number.isFinite(n) || n < 0) {
        console.error(red(`--days expects a non-negative number, got: ${argv[i] ?? "(nothing)"}`));
        return 2;
      }
      days = n;
    } else {
      console.error(red(`unknown prune-backups option: ${argv[i]}\n`));
      console.error(USAGE);
      return 2;
    }
  }
  const res = await pruneStackBackups(days, logLine);
  if (res.deleted.length === 0 && res.kept.length === 0) {
    console.log(dim("no backup branches found"));
    return 0;
  }
  const keptNote = res.kept.length > 0 ? dim(` (${res.kept.length} kept)`) : "";
  console.log(green(`✓ deleted ${bold(String(res.deleted.length))} backup branch(es)`) + keptNote);
  return 0;
}

/** The current worktree's branch, for the bare `wt restack` form. */
async function branchFromCwd(): Promise<string | null> {
  try {
    const branch = (await git(["rev-parse", "--abbrev-ref", "HEAD"], process.cwd())).trim();
    return branch && branch !== "HEAD" ? branch : null;
  } catch {
    return null;
  }
}

export async function run(argv: string[]): Promise<number> {
  const [first] = argv;
  if (first === "--help" || first === "-h") {
    console.log(USAGE);
    return 0;
  }
  if (first === "prune-backups") {
    return runPruneBackups(argv.slice(1));
  }
  let branch: string | undefined;
  let onto: string | undefined;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === "--onto") {
      onto = argv[++i];
      if (!onto) {
        console.error(red("--onto requires a ref"));
        return 2;
      }
    } else if (a.startsWith("--")) {
      console.error(red(`unknown flag: ${a}\n`));
      console.error(USAGE);
      return 2;
    } else if (!branch) branch = a;
    else {
      console.error(red(`unexpected arg: ${a}\n`));
      console.error(USAGE);
      return 2;
    }
  }
  if (!branch) {
    branch = (await branchFromCwd()) ?? undefined;
    if (branch) console.log(dim(`restacking from current branch ${branch}`));
  }
  if (!branch) {
    console.error(red(`${USAGE}\n  (no branch given and the cwd isn't on one)`));
    return 2;
  }
  const opts = onto ? { onto } : {};
  return report(branch, await rebaseStack(branch, opts, logLine));
}
