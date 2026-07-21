import { config } from "../../core/config.ts";
import { HARNESSES, type HarnessId } from "../../core/harness/index.ts";
import { attachOrCreate } from "../../core/tmux.ts";
import type { SessionShortcut } from "../../core/tmux/naming.ts";
import { listWorktrees } from "../../core/worktree.ts";

const TARGETS = new Set<SessionShortcut>(["shell", "diff", "harness"]);

/**
 * Interactive entrypoint used over SSH for one remote worktree. The Mac owns
 * the wt UI; this command owns only the selected row's remote tmux client.
 */
export async function run(argv: string[]): Promise<number> {
  const [slug, rawTarget, rawHarness] = argv;
  if (!slug || !rawTarget || !TARGETS.has(rawTarget as SessionShortcut)) {
    console.error("usage: wt _session <slug> <shell|diff|harness> [harness]");
    return 2;
  }
  const harnessId = (rawHarness ?? "codex") as HarnessId;
  if (!HARNESSES.some((h) => h.id === harnessId)) {
    console.error(`unknown harness: ${rawHarness}`);
    return 2;
  }
  const worktree = (await listWorktrees()).find((wt) => !wt.isMain && wt.slug === slug);
  if (!worktree) {
    console.error(`remote worktree not found: ${slug}`);
    return 1;
  }

  let target = rawTarget as SessionShortcut;
  for (;;) {
    const kind = target === "harness" ? harnessId : target;
    const result = await attachOrCreate({
      slug,
      cwd: worktree.path,
      kind,
      base: `origin/${config.branch.base}`,
    });
    if (result.kind === "switch") {
      target = result.target;
      continue;
    }
    if (result.kind === "spawn-failed") {
      console.error(result.reason);
      return 1;
    }
    if (result.kind === "exited" && result.stderr) console.error(result.stderr);
    return 0;
  }
}
