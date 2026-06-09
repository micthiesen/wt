import { existsSync, readdirSync, rmSync, statSync } from "node:fs";
import { join } from "node:path";

import { config } from "./config.ts";
import { createLogger } from "./logger.ts";

const log = createLogger("[logs]");

/**
 * Newest `<slug>-*.log` under `config.paths.logDir` by mtime, or null
 * if none. Used to find the tail target for a worktree without needing
 * the lock meta to record the log path — the log file may outlive the
 * lock.
 */
export function latestLogFor(slug: string): string | null {
  const dir = config.paths.logDir;
  if (!existsSync(dir)) return null;
  const prefix = `${slug}-`;
  let best: { path: string; mtime: number } | null = null;
  for (const name of readdirSync(dir)) {
    if (!name.startsWith(prefix) || !name.endsWith(".log")) continue;
    const path = join(dir, name);
    // The file can vanish between readdir and stat (startup reap, manual
    // cleanup) — this runs on a polling path, so skip rather than throw.
    let mtime: number;
    try {
      mtime = statSync(path).mtimeMs;
    } catch {
      continue;
    }
    if (!best || mtime > best.mtime) best = { path, mtime };
  }
  return best?.path ?? null;
}

/**
 * Drop `<slug>-*.log` destroy-log files whose slug isn't in `liveSlugs`.
 * Called from startup reap so the dir doesn't accumulate ghosts from
 * worktrees the user removed long ago. Live-slug logs are kept
 * regardless of age — the user might be tailing them via `wt logs
 * <slug>` while a destroy is in flight.
 *
 * Errors are swallowed: a missing dir, a permission glitch, or a
 * filename that doesn't match the expected shape are all best-effort
 * skips. An accumulated log is a worse outcome than blocking startup.
 */
export function reapDestroyLogs(liveSlugs: ReadonlySet<string>): void {
  const dir = config.paths.logDir;
  if (!existsSync(dir)) return;
  let names: string[];
  try {
    names = readdirSync(dir);
  } catch {
    return;
  }
  let removed = 0;
  for (const name of names) {
    if (!name.endsWith(".log")) continue;
    // `<slug>-<iso>.log` — split on the last `-` that precedes the
    // timestamp. The iso stamp is `YYYY-MM-DDTHH-MM-SS-mmmZ` per
    // `spawnBackgroundRemove`, so the slug is everything before the
    // first `-YYYY-…` chunk. Match that explicitly to avoid
    // misclassifying a slug that itself contains `-` (most do).
    const m = /^(.+)-\d{4}-\d{2}-\d{2}T/.exec(name);
    if (!m) continue;
    const slug = m[1]!;
    if (liveSlugs.has(slug)) continue;
    const path = join(dir, name);
    try {
      rmSync(path, { force: true });
      removed++;
    } catch (err) {
      log.warn("destroy log reap failed", {
        path,
        err: err instanceof Error ? err.message : String(err),
      });
    }
  }
  if (removed > 0) log.info("reaped destroy logs", { removed });
}
