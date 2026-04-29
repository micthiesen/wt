import { removeWorktree } from "../../core/lifecycle.ts";
import { listWorktrees } from "../../core/worktree.ts";

type Parsed = {
  slug: string;
  force: boolean;
  destroyStage: boolean;
  deleteBranch: boolean;
};

function boolArg(v: string | undefined): boolean {
  if (!v) return false;
  const s = v.toLowerCase();
  return s === "true" || s === "1" || s === "yes";
}

function parse(argv: string[]): Parsed | { error: string } {
  let slug: string | undefined;
  let force = false;
  let destroyStage = false;
  let deleteBranch = false;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === "--force") force = boolArg(argv[++i]);
    else if (a === "--destroy-stage") destroyStage = boolArg(argv[++i]);
    else if (a === "--delete-branch") deleteBranch = boolArg(argv[++i]);
    else if (!slug) slug = a;
    else return { error: `unexpected arg: ${a}` };
  }
  if (!slug) return { error: "missing slug" };
  return { slug, force, destroyStage, deleteBranch };
}

/**
 * Background destroy entry point. The parent (`spawnBackgroundRemove`)
 * redirects our stdout+stderr to the log file at spawn time, so every
 * `console.log` here — and every grandchild's output — lands in the log
 * automatically. No monkey-patching.
 */
export async function run(argv: string[]): Promise<number> {
  const parsed = parse(argv);
  if ("error" in parsed) {
    console.error(parsed.error);
    return 2;
  }

  const wt = (await listWorktrees()).find((w) => w.slug === parsed.slug);
  if (!wt) {
    console.error(`No worktree: ${parsed.slug}`);
    return 1;
  }
  console.log(
    `[bg destroy] slug=${parsed.slug} force=${parsed.force} ` +
      `stage=${parsed.destroyStage} branch=${parsed.deleteBranch}`,
  );
  const result = await removeWorktree(wt, {
    force: parsed.force,
    destroyStage: parsed.destroyStage,
    deleteBranch: parsed.deleteBranch,
    onLog: (line) => console.log(line),
    onPhase: (phase) => console.log(`· ${phase}`),
  });
  if (!result.ok) {
    console.error(`failed: ${result.message}`);
    return 1;
  }
  console.log(`✓ ${result.message}`);
  if (result.destroyedStage) console.log(`✓ destroyed stage ${wt.stage}`);
  if (result.deletedBranch) console.log(`✓ deleted branch ${wt.branch}`);
  return 0;
}
