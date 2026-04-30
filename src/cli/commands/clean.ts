import { removeWorktree, spawnBackgroundRemove } from "../../core/lifecycle.ts";
import type { Status, Worktree } from "../../core/types.ts";
import { StatusKind } from "../../core/types.ts";
import {
  fetchOrigin,
  isDeployed,
  listWorktrees,
  worktreeStatus,
} from "../../core/worktree.ts";
import { bold, cyan, dim, green, red, yellow } from "../colors.ts";
import { confirm, isInteractive } from "../prompt.ts";

type Flags = {
  yes: boolean;
  destroyStage: boolean | null;
  background: boolean;
};

function parse(argv: string[]): Flags | { error: string } {
  let yes = false;
  let destroyStage: boolean | null = null;
  let background = true;
  for (const a of argv) {
    if (a === "--yes" || a === "-y") yes = true;
    else if (a === "--destroy-stage") destroyStage = true;
    else if (a === "--no-destroy-stage") destroyStage = false;
    else if (a === "--background") background = true;
    else if (a === "--foreground") background = false;
    else return { error: `unknown flag: ${a}` };
  }
  return { yes, destroyStage, background };
}

export async function run(argv: string[]): Promise<number> {
  const parsed = parse(argv);
  if ("error" in parsed) {
    console.error(red(parsed.error));
    return 2;
  }

  console.log(dim("Fetching origin..."));
  await fetchOrigin();

  const wts = (await listWorktrees()).filter((w) => !w.isMain && w.branch);

  const candidates: [Worktree, Status][] = [];
  const skipped: [Worktree, Status][] = [];
  for (const w of wts) {
    const st = await worktreeStatus(w);
    if (st.kind === StatusKind.Busy) skipped.push([w, st]);
    else if (st.kind === StatusKind.Merged || st.kind === StatusKind.Gone)
      candidates.push([w, st]);
  }

  if (skipped.length) {
    console.log(dim("Skipping (already in progress):"));
    for (const [w, st] of skipped) {
      const age = st.age ? dim(` (${st.age})`) : "";
      console.log(
        `  ${cyan(w.slug)} — ${yellow(st.label)}${age}  ${dim(`wt logs ${w.slug}`)}`,
      );
    }
  }

  if (candidates.length === 0) {
    console.log(green("Nothing to clean."));
    return 0;
  }

  console.log(bold("Cleanup candidates:"));
  for (const [w, st] of candidates) {
    const tag =
      st.kind === StatusKind.Merged
        ? green("merged")
        : yellow("gone (squash-merged or force-deleted)");
    console.log(`  ${cyan(w.slug.padEnd(40))}  ${tag}  ${dim(w.branch)}`);
  }

  if (!parsed.yes) {
    if (!isInteractive()) {
      console.error(red("Confirming clean requires a TTY. Pass -y."));
      return 2;
    }
    if (!(await confirm(`Remove ${candidates.length}?`, true))) return 0;
  }

  for (const [w] of candidates) {
    // Resolve destroyStage per worktree using the Python semantics:
    // explicit flag wins; default is "destroy if deployed".
    const destroy =
      parsed.destroyStage !== null ? parsed.destroyStage : isDeployed(w.path);

    if (parsed.background) {
      const logPath = spawnBackgroundRemove(w.slug, {
        force: false,
        destroyStage: destroy,
        deleteBranch: true,
      });
      console.log(green(`✓ dispatched ${w.slug}`) + dim(` → ${logPath}`));
    } else {
      const result = await removeWorktree(w, {
        force: false,
        destroyStage: destroy,
        deleteBranch: true,
        onLog: (line) => console.log(dim(`  ${line}`)),
        onPhase: (phase) => console.log(dim(`· ${phase}`)),
      });
      if (result.ok) console.log(green(`✓ ${result.message}`));
      else console.log(red(`✗ ${w.slug}: ${result.message}`));
    }
  }
  return 0;
}
