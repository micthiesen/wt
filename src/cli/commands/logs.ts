import { existsSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

import { latestLogFor } from "../../core/logs.ts";
import { LOG_DIR } from "../../core/paths.ts";
import { listWorktrees } from "../../core/worktree.ts";
import { dim, red } from "../colors.ts";

/** Newest log across *any* slug — used for `wt logs` with no arg. */
function mostRecentLog(): string | null {
  if (!existsSync(LOG_DIR)) return null;
  let files: string[];
  try {
    files = readdirSync(LOG_DIR);
  } catch {
    return null;
  }
  const matching = files
    .filter((f) => f.endsWith(".log"))
    .map((f) => ({ name: f, mtime: statSync(join(LOG_DIR, f)).mtimeMs }))
    .sort((a, b) => b.mtime - a.mtime);
  return matching[0] ? join(LOG_DIR, matching[0].name) : null;
}

export async function run(argv: string[]): Promise<number> {
  const slug = argv.find((a) => !a.startsWith("-")) ?? null;

  let logPath: string | null = null;
  if (slug) {
    const wts = await listWorktrees();
    const match = wts.find((w) => w.slug === slug);
    if (match) logPath = latestLogFor(match.slug);
  } else {
    logPath = mostRecentLog();
  }
  if (!logPath) {
    console.log(dim("No destroy logs found."));
    return 1;
  }
  if (!existsSync(logPath)) {
    console.error(red(`Log file missing: ${logPath}`));
    return 1;
  }
  console.log(dim(`→ ${logPath}`));
  const p = Bun.spawn(["tail", "-n", "200", "-F", logPath], {
    stdout: "inherit",
    stderr: "inherit",
  });
  return (await p.exited) ?? 0;
}
