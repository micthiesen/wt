import { existsSync, readdirSync, statSync } from "node:fs";
import { join } from "node:path";

import { LOG_DIR } from "./paths.ts";

/**
 * Newest `<slug>-*.log` under `LOG_DIR` by mtime, or null if none.
 * Used to find the tail target for a worktree without needing the lock
 * meta to record the log path — the log file may outlive the lock.
 */
export function latestLogFor(slug: string): string | null {
  if (!existsSync(LOG_DIR)) return null;
  const prefix = `${slug}-`;
  let best: { path: string; mtime: number } | null = null;
  for (const name of readdirSync(LOG_DIR)) {
    if (!name.startsWith(prefix) || !name.endsWith(".log")) continue;
    const path = join(LOG_DIR, name);
    const mtime = statSync(path).mtimeMs;
    if (!best || mtime > best.mtime) best = { path, mtime };
  }
  return best?.path ?? null;
}
