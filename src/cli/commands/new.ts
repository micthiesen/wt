import { createWorktree, parseInput } from "../../core/lifecycle.ts";
import { listWorktrees } from "../../core/worktree.ts";
import { bold, cyan, dim, green, red, yellow } from "../colors.ts";
import { ask, isInteractive, pickIndex } from "../prompt.ts";
import { openInZed } from "../../core/zed.ts";

type Flags = {
  slug?: string;
  open: boolean; // default: tty
  install: boolean;
  raw?: string;
  any: boolean;
  base?: string;
};

function parse(argv: string[]): Flags | { error: string } {
  let slug: string | undefined;
  let noOpen = false;
  let noInstall = false;
  let raw: string | undefined;
  let any = false;
  let base: string | undefined;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === "--slug") slug = argv[++i];
    else if (a === "--no-open") noOpen = true;
    else if (a === "--open") noOpen = false;
    else if (a === "--no-install") noInstall = true;
    else if (a === "--any") any = true;
    else if (a === "--base") base = argv[++i];
    else if (a.startsWith("--")) return { error: `unknown flag: ${a}` };
    else if (!raw) raw = a;
    else return { error: `unexpected arg: ${a}` };
  }
  if (base !== undefined && !base) return { error: "--base requires a ref" };
  return {
    slug,
    open: !noOpen && isInteractive(),
    install: !noInstall,
    raw,
    any,
    base,
  };
}

export async function run(argv: string[]): Promise<number> {
  const parsed = parse(argv);
  if ("error" in parsed) {
    console.error(red(parsed.error));
    return 2;
  }
  if (!parsed.raw) {
    console.error(
      red(
        "usage: wt new <linear-url|id|branch|slug> [--slug s] [--any] [--base ref] [--no-open] [--no-install]",
      ),
    );
    return 2;
  }

  let branch: string;
  try {
    branch = await parseInput(parsed.raw, {
      slugHint: parsed.slug,
      anyAuthor: parsed.any,
      promptForSlug: isInteractive()
        ? async (id) => ask(`slug for ${id}: `)
        : undefined,
      promptForChoice: isInteractive()
        ? async (id, branches) => {
            const idx = await pickIndex(branches, `Multiple branches for ${id}:`);
            return idx === null ? null : branches[idx]!;
          }
        : undefined,
    });
  } catch (e) {
    console.error(red(e instanceof Error ? e.message : String(e)));
    return 1;
  }

  // Short-circuit if the branch already has a worktree.
  const existing = (await listWorktrees()).find((w) => !w.isMain && w.branch === branch);
  if (existing) {
    console.log(yellow(`Worktree already exists for ${branch}`));
    console.log(`  ${dim("path:")}  ${existing.path}`);
    console.log(`  ${dim("stage:")} ${existing.stage}`);
    if (parsed.open) await openInZed(existing.path);
    return 0;
  }

  const result = await createWorktree(branch, {
    runInstall: parsed.install,
    base: parsed.base,
    onLog: (line) => console.log(dim(line)),
    onPhase: (phase) => console.log(dim(`· ${phase}`)),
  });

  if (!result.ok) {
    console.error(red(result.reason));
    return 1;
  }

  console.log(green(`✓ created ${bold(cyan(result.slug))}`));
  console.log(`  ${dim("path:")}  ${result.path}`);
  console.log(`  ${dim("stage:")} ${result.stage}`);

  if (parsed.open) await openInZed(result.path);
  return 0;
}
