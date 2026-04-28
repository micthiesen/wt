import { spawn } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

/**
 * Zed 0.20x changed `zed <path>` to reuse the current window instead of
 * focusing the one that already has <path> open. Zed's CLI has no flag
 * for the old smart-focus behavior, and its SQLite `workspaces` table
 * records its own internal `window_id` which isn't the macOS AX id, so
 * we can't map DB → yabai directly.
 *
 * Workaround: track the yabai window id we got on each `zed -n` spawn.
 * Next time the user asks to open the same path, we focus by id via
 * yabai, which is the only way to pick a specific window when Zed's AX
 * titles collide (which they do in practice right after an update).
 */

const CACHE_FILE = join(homedir(), ".cache", "wt", "zed-windows.json");

type CacheEntry = { windowId: number; lastSeen: string };
type CacheFile = { byPath: Record<string, CacheEntry> };

type YabaiWindow = {
  id: number;
  pid: number;
  app: string;
};

function readCache(): CacheFile {
  if (!existsSync(CACHE_FILE)) return { byPath: {} };
  try {
    const raw = readFileSync(CACHE_FILE, "utf8");
    const data = JSON.parse(raw) as CacheFile;
    return { byPath: data?.byPath ?? {} };
  } catch {
    return { byPath: {} };
  }
}

function writeCache(cache: CacheFile): void {
  mkdirSync(dirname(CACHE_FILE), { recursive: true });
  writeFileSync(CACHE_FILE, `${JSON.stringify(cache, null, 2)}\n`);
}

function yabaiQueryAllWindows(): YabaiWindow[] | null {
  const r = Bun.spawnSync(["yabai", "-m", "query", "--windows"]);
  if (r.exitCode !== 0) return null;
  try {
    const parsed = JSON.parse(r.stdout.toString()) as YabaiWindow[];
    return Array.isArray(parsed) ? parsed : null;
  } catch {
    return null;
  }
}

function zedWindowIds(): Set<number> {
  const all = yabaiQueryAllWindows();
  if (!all) return new Set();
  return new Set(all.filter((w) => w.app === "Zed").map((w) => w.id));
}

function yabaiWindowExists(id: number): boolean {
  const r = Bun.spawnSync([
    "yabai",
    "-m",
    "query",
    "--windows",
    "--window",
    String(id),
  ]);
  if (r.exitCode !== 0) return false;
  try {
    const w = JSON.parse(r.stdout.toString()) as YabaiWindow;
    return w?.app === "Zed";
  } catch {
    return false;
  }
}

function yabaiFocus(id: number): boolean {
  const r = Bun.spawnSync(["yabai", "-m", "window", "--focus", String(id)]);
  return r.exitCode === 0;
}

/**
 * Resolve a worktree path to its tracked yabai window id, if one is
 * known and still alive. Prunes the cache on miss so stale entries
 * don't accumulate.
 */
export function findZedWindowForPath(path: string): number | null {
  const cache = readCache();
  const entry = cache.byPath[path];
  if (!entry) return null;
  if (yabaiWindowExists(entry.windowId)) return entry.windowId;
  delete cache.byPath[path];
  writeCache(cache);
  return null;
}

export function focusYabaiWindow(id: number): boolean {
  return yabaiFocus(id);
}

/**
 * Spawn a detached `zed -n <path>`, then poll yabai until a new Zed
 * window appears (one not present in `beforeIds`). Records the id in
 * the cache so the next lookup for this path focuses it.
 *
 * Returns once we either find the new window id or give up. Uses
 * `child_process.spawn` so the caller can stay sync-ish; the tracking
 * work runs in a microtask-ish poll loop via `setTimeout`.
 */
export async function spawnZedAndTrack(path: string): Promise<void> {
  const beforeIds = zedWindowIds();
  const child = spawn("zed", ["-n", path], {
    stdio: "ignore",
    detached: true,
  });
  child.unref();

  // Zed needs ~100–500ms to register the new window with the AX tree.
  // Poll for up to ~3s, then give up — worst case the user gets no
  // tracking for this open and we retry next time.
  const deadline = Date.now() + 3000;
  while (Date.now() < deadline) {
    await new Promise((r) => setTimeout(r, 150));
    const now = zedWindowIds();
    for (const id of now) {
      if (!beforeIds.has(id)) {
        const cache = readCache();
        cache.byPath[path] = {
          windowId: id,
          lastSeen: new Date().toISOString(),
        };
        writeCache(cache);
        return;
      }
    }
  }
}
