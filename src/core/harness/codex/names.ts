/**
 * Stable wt-friendly names for Codex sessions. Codex generates session
 * UUIDs itself and has no launch-time naming flag, so wt assigns names
 * after discovering new interactive rollouts and persists the mapping at
 * `~/.cache/wt/codex-sessions.json`.
 *
 * Shape: `{ <slug>: { <session-id>: "<friendly-name>" } }`.
 * The first discovered session is `primary`; later sessions are `2`,
 * `3`, and so on. UUIDs remain the authoritative resume handles.
 */
import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

import { withFileLock } from "../../locks.ts";
import { createLogger } from "../../logger.ts";

const STATE_FILE = join(homedir(), ".cache", "wt", "codex-sessions.json");
const log = createLogger("[codex-sessions]");

type FileShape = Record<string, Record<string, string>>;

function readFile(): FileShape {
  if (!existsSync(STATE_FILE)) return {};
  try {
    const parsed = JSON.parse(readFileSync(STATE_FILE, "utf8")) as unknown;
    if (!parsed || typeof parsed !== "object") return {};
    const out: FileShape = {};
    for (const [slug, rawNames] of Object.entries(
      parsed as Record<string, unknown>,
    )) {
      if (!rawNames || typeof rawNames !== "object") continue;
      const names: Record<string, string> = {};
      for (const [sessionId, name] of Object.entries(
        rawNames as Record<string, unknown>,
      )) {
        if (typeof name === "string" && name.length > 0) {
          names[sessionId] = name;
        }
      }
      if (Object.keys(names).length > 0) out[slug] = names;
    }
    return out;
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { file: STATE_FILE });
    return {};
  }
}

function writeFile(shape: FileShape): void {
  try {
    mkdirSync(dirname(STATE_FILE), { recursive: true });
    const tmp = `${STATE_FILE}.${process.pid}.tmp`;
    writeFileSync(tmp, `${JSON.stringify(shape, null, 2)}\n`);
    renameSync(tmp, STATE_FILE);
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { file: STATE_FILE });
    throw err;
  }
}

function withNamesLock<T>(fn: () => T): T {
  return withFileLock("__codex_names__", fn);
}

function assignMissingNames(
  existing: Readonly<Record<string, string>>,
  sessionIds: readonly string[],
): { names: Record<string, string>; changed: boolean } {
  const names = { ...existing };
  const used = new Set(Object.values(names));
  let changed = false;

  for (const sessionId of sessionIds) {
    if (names[sessionId]) continue;
    let name: string;
    if (!used.has("primary")) {
      name = "primary";
    } else {
      let n = 2;
      while (used.has(`${n}`)) n++;
      name = `${n}`;
    }
    names[sessionId] = name;
    used.add(name);
    changed = true;
  }
  return { names, changed };
}

/**
 * Return stable names for `sessionIds`, assigning any newly-discovered
 * sessions in the supplied order. Discovery passes newest first, so an
 * existing worktree's most recent session becomes `primary` on migration.
 */
export function reconcileCodexNames(
  slug: string,
  sessionIds: readonly string[],
): Readonly<Record<string, string>> {
  try {
    return withNamesLock(() => {
      const shape = readFile();
      const { names, changed } = assignMissingNames(
        shape[slug] ?? {},
        sessionIds,
      );
      if (changed) {
        shape[slug] = names;
        writeFile(shape);
      }
      return names;
    });
  } catch (err) {
    // Friendly naming must never make session discovery unavailable.
    // Fall back to this scan's stable ordering; a later successful poll
    // will persist the same assignments.
    log.error(err instanceof Error ? err : String(err), { file: STATE_FILE });
    return assignMissingNames({}, sessionIds).names;
  }
}

/** Drop every Codex friendly name for a destroyed worktree. */
export function clearCodexNames(slug: string): void {
  withNamesLock(() => {
    const shape = readFile();
    if (!(slug in shape)) return;
    delete shape[slug];
    writeFile(shape);
  });
}

/** Reap stale slug entries at startup. */
export function reapCodexNames(liveSlugs: ReadonlySet<string>): void {
  withNamesLock(() => {
    const shape = readFile();
    let changed = false;
    for (const slug of Object.keys(shape)) {
      if (!liveSlugs.has(slug)) {
        delete shape[slug];
        changed = true;
      }
    }
    if (changed) writeFile(shape);
  });
}
