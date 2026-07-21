import { branchExists } from "../../core/git.ts";
import {
  removeWorktree,
  spawnBackgroundRemove,
} from "../../core/lifecycle.ts";
import { lockAge, lockLabel, lockStatus } from "../../core/locks.ts";
import { latestLogFor } from "../../core/logs.ts";
import { isOurStageDeployed } from "../../core/stage-safety.ts";
import { killAllSessionsFor } from "../../core/tmux.ts";
import type { Worktree } from "../../core/types.ts";
import {
  listWorktrees,
  unpushedCommits,
  worktreeIsDirty,
} from "../../core/worktree.ts";
import { bold, dim, green, red, yellow } from "../colors.ts";
import { confirm, isInteractive, pickIndex } from "../prompt.ts";

type Flags = {
  slug?: string;
  yes: boolean;
  force: boolean;
  destroyStage: boolean | null;
  deleteBranch: boolean | null;
  background: boolean;
};

function parse(argv: string[]): Flags | { error: string } {
  let slug: string | undefined;
  let yes = false;
  let force = false;
  let destroyStage: boolean | null = null;
  let deleteBranch: boolean | null = null;
  let background = false;
  for (const a of argv) {
    if (a === "--yes" || a === "-y") yes = true;
    else if (a === "--force") force = true;
    else if (a === "--destroy-stage") destroyStage = true;
    else if (a === "--no-destroy-stage") destroyStage = false;
    else if (a === "--delete-branch") deleteBranch = true;
    else if (a === "--keep-branch") deleteBranch = false;
    else if (a === "--background" || a === "-b") background = true;
    else if (a.startsWith("--") || a.startsWith("-")) return { error: `unknown flag: ${a}` };
    else if (!slug) slug = a;
    else return { error: `unexpected arg: ${a}` };
  }
  return { slug, yes, force, destroyStage, deleteBranch, background };
}

async function decideDestroyStage(
  wt: Worktree,
  flag: boolean | null,
  yes: boolean,
): Promise<boolean> {
  if (flag === true) return true;
  if (flag === false) return false;
  // `isOurStageDeployed` is the strict check — outputs.json must
  // mention the owned (pinned, prefix-valid) stage, otherwise we treat
  // the worktree as not-deployed-by-us (a foreign deploy in this
  // directory will not trigger the prompt). `removeWorktree`
  // re-validates via `safeStage` before shelling out.
  if (!isOurStageDeployed(wt)) return false;
  if (yes) return true;
  if (isInteractive()) {
    return confirm(
      `Stage ${bold(wt.stage)} looks deployed (.sst/outputs.json has live outputs). Run \`sst remove\`?`,
      true,
    );
  }
  console.log(
    yellow(`Skipping sst remove for ${wt.stage} (non-interactive; pass --destroy-stage to run it)`),
  );
  return false;
}

async function decideDeleteBranch(
  wt: Worktree,
  flag: boolean | null,
): Promise<boolean> {
  // The user already asked to remove this worktree; dirty/unpushed work
  // is caught upstream by the tree-clean check. Default to deleting the
  // branch — pass --keep-branch to opt out.
  if (!wt.branch || !(await branchExists(wt.branch))) return false;
  if (flag !== null) return flag;
  return true;
}

export async function run(argv: string[]): Promise<number> {
  const parsed = parse(argv);
  if ("error" in parsed) {
    console.error(red(parsed.error));
    return 2;
  }

  const wts = (await listWorktrees()).filter((w) => !w.isMain);
  if (wts.length === 0) {
    console.log(yellow("No worktrees to remove."));
    return 0;
  }

  let target: Worktree | undefined;
  if (parsed.slug) {
    target = wts.find((w) => w.slug === parsed.slug);
    if (!target) {
      console.error(red(`No worktree with slug: ${parsed.slug}`));
      return 1;
    }
  } else {
    if (!isInteractive()) {
      console.error(red("Picking a worktree requires a TTY."));
      return 2;
    }
    const idx = await pickIndex(
      wts.map((w) => w.slug),
      "Remove which worktree?",
    );
    if (idx === null) return 0;
    target = wts[idx];
  }
  if (!target) return 1;

  // Busy check — surface the holder and bail.
  const lock = lockStatus(target.slug);
  if (lock) {
    const age = lockAge(lock);
    console.log(
      yellow(`${target.slug} is busy: ${lockLabel(lock)}${age ? ` (${age})` : ""}`),
    );
    const logPath = latestLogFor(target.slug);
    if (logPath) console.log(dim(`  log: ${logPath}`));
    return 1;
  }

  let force = parsed.force;
  if (!force) {
    const dirty = await worktreeIsDirty(target.path);
    const unpushed = dirty ? 0 : await unpushedCommits(target.path);
    if (dirty || unpushed > 0) {
      const reason = dirty
        ? "uncommitted changes"
        : `${unpushed} unpushed commit${unpushed === 1 ? "" : "s"}`;
      console.log(yellow(`${target.slug}: ${reason}`));
      if (parsed.yes) {
        console.error(red("Refusing to remove without --force."));
        return 1;
      }
      if (!isInteractive()) {
        console.error(red("Pass --force (or --yes with --force) to remove anyway."));
        return 1;
      }
      if (!(await confirm("Remove anyway?", false))) return 0;
      force = true;
    }
  }

  const destroyStage = await decideDestroyStage(target, parsed.destroyStage, parsed.yes);
  const deleteBranch = await decideDeleteBranch(target, parsed.deleteBranch);

  // CLI callers do not have the TUI destroy flow's session teardown. In
  // particular, SSH-backed rows enter remote shell/diff/harness sessions via
  // this process but delete through `wt rm`, so clean them up here before the
  // checkout disappears. This is idempotent when the TUI already did it.
  await killAllSessionsFor(target.slug);

  if (parsed.background) {
    const logPath = spawnBackgroundRemove(target.slug, {
      force,
      destroyStage,
      deleteBranch,
    });
    console.log(green(`✓ dispatched destroy of ${bold(target.slug)} ${dim("in background")}`));
    console.log(dim(`  → log: ${logPath}`));
    console.log(dim(`  → tail with `) + bold(`wt logs ${target.slug}`));
    return 0;
  }

  const result = await removeWorktree(target, {
    force,
    destroyStage,
    deleteBranch,
    onLog: (line) => console.log(dim(line)),
    onPhase: (phase) => console.log(dim(`· ${phase}`)),
  });

  if (!result.ok) {
    console.error(red(`Failed: ${result.message}`));
    if (!force) {
      console.log(
        dim(`  retry with `) + bold(`wt rm ${target.slug} --force`) + dim(" to override"),
      );
    }
    return 1;
  }
  console.log(green(`✓ ${result.message}`));
  if (result.destroyedStage) console.log(green(`✓ destroyed stage ${target.stage}`));
  if (result.deletedBranch) console.log(green(`✓ deleted branch ${target.branch}`));
  return 0;
}
