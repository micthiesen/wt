import { config } from "../../core/config.ts";
import { gitRun, revParse } from "../../core/git.ts";
import { readWtState, setSlugBase } from "../../core/wtstate.ts";
import { listWorktrees } from "../../core/worktree.ts";
import type { Worktree } from "../../core/types.ts";
import { dim, green, red, yellow } from "../colors.ts";

const USAGE = `usage: wt base <slug>                show the recorded fork base
       wt base set <slug> <ref>     record <ref> as the fork base
       wt base clear <slug>         forget the recorded fork base

The fork base is what \`wt new --base <ref>\` records: the branch a
worktree is based on. It is THE stack primitive — worktrees whose
records chain into each other render as a stack, diff against their
parent, and replay onto it on \`wt restack\`. \`set\` exists for
backfill — worktrees created before recording existed, or whose base
changed by hand.`;

async function findWorktree(slug: string): Promise<Worktree | null> {
  const wts = (await listWorktrees()).filter((w) => !w.isMain);
  return wts.find((w) => w.slug === slug) ?? null;
}

function show(slug: string): number {
  const entry = readWtState().slugs[slug];
  if (!entry?.baseBranch) {
    console.log(dim(`${slug}: no recorded fork base (diffs against ${config.branch.base})`));
    return 0;
  }
  console.log(`${slug}: ${entry.baseBranch}${entry.baseSha ? dim(` @ ${entry.baseSha.slice(0, 12)}`) : ""}`);
  return 0;
}

async function set(slug: string, ref: string): Promise<number> {
  const wt = await findWorktree(slug);
  if (!wt) {
    console.error(red(`no worktree: ${slug}`));
    return 1;
  }
  const branch = ref.replace(/^origin\//, "");
  if (branch === config.branch.base) {
    console.error(red(`${branch} is trunk — that's the default; use \`wt base clear\` instead`));
    return 2;
  }
  if (branch === wt.branch) {
    console.error(red(`${branch} is ${slug}'s own branch — a worktree can't be based on itself`));
    return 2;
  }
  if (!(await revParse(ref)) && !(await revParse(`origin/${branch}`))) {
    console.error(red(`ref does not resolve: ${ref}`));
    return 1;
  }
  // Anchor at the fork point, not the base's current tip — the base may
  // have advanced since the fork. Best-effort; the branch name alone is
  // enough for display/diff.
  const mb = await gitRun(["merge-base", wt.branch, ref], wt.path);
  const sha = mb.exitCode === 0 ? mb.stdout.trim() : "";
  setSlugBase(slug, { branch, sha: sha || undefined });
  console.log(green(`✓ ${slug} base → ${branch}${sha ? dim(` @ ${sha.slice(0, 12)}`) : ""}`));
  console.log(dim("restart wt (or wait for the next state refresh) to see it in the TUI"));
  return 0;
}

function clear(slug: string): number {
  const entry = readWtState().slugs[slug];
  if (!entry?.baseBranch) {
    console.log(yellow(`${slug}: nothing recorded`));
    return 0;
  }
  setSlugBase(slug, null);
  console.log(green(`✓ cleared — ${slug} diffs against ${config.branch.base} again`));
  return 0;
}

export async function run(argv: string[]): Promise<number> {
  const [first, ...rest] = argv;
  if (!first || first === "--help" || first === "-h") {
    console.log(USAGE);
    return first ? 0 : 2;
  }
  if (first === "set") {
    const [slug, ref] = rest;
    if (!slug || !ref) {
      console.error(red(USAGE));
      return 2;
    }
    return set(slug, ref);
  }
  if (first === "clear") {
    const [slug] = rest;
    if (!slug) {
      console.error(red(USAGE));
      return 2;
    }
    return clear(slug);
  }
  if (rest.length > 0) {
    console.error(red(USAGE));
    return 2;
  }
  return show(first);
}
