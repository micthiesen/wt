import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

import { createLogger } from "./logger.ts";

const ARCHIVE_FILE = join(homedir(), ".cache", "wt", "archive.json");
const log = createLogger("[archive]");

type ArchiveFile = { slugs: string[] };

/**
 * Read the archived-slug set from disk. Returns an empty set on any
 * IO/parse error so the TUI degrades to "nothing archived" rather
 * than crashing on a corrupt file.
 */
export function readArchived(): Set<string> {
  if (!existsSync(ARCHIVE_FILE)) return new Set();
  try {
    const raw = readFileSync(ARCHIVE_FILE, "utf8");
    const data = JSON.parse(raw) as ArchiveFile;
    return new Set(Array.isArray(data?.slugs) ? data.slugs : []);
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { file: ARCHIVE_FILE });
    return new Set();
  }
}

function writeArchived(set: Set<string>): void {
  mkdirSync(dirname(ARCHIVE_FILE), { recursive: true });
  const data: ArchiveFile = { slugs: [...set].sort() };
  writeFileSync(ARCHIVE_FILE, `${JSON.stringify(data, null, 2)}\n`);
}

/** Flip the archived flag for a slug; returns the new state. */
export function toggleArchived(slug: string): { archived: boolean } {
  const set = readArchived();
  const wasArchived = set.has(slug);
  if (wasArchived) set.delete(slug);
  else set.add(slug);
  writeArchived(set);
  return { archived: !wasArchived };
}

/**
 * Idempotent "archive this slug". No-op if already archived; otherwise
 * adds and writes. Used by the TUI's remove/clean flows to tuck the
 * row into the archived section for the duration of the destroy.
 */
export function archiveSlug(slug: string): void {
  const set = readArchived();
  if (set.has(slug)) return;
  set.add(slug);
  writeArchived(set);
}

/**
 * Drop a slug from the archived set. Called by `createWorktree` so a
 * fresh worktree with a previously-destroyed slug starts un-archived.
 * No-op if the slug wasn't archived.
 */
export function clearArchived(slug: string): void {
  const set = readArchived();
  if (!set.delete(slug)) return;
  writeArchived(set);
}

/**
 * Drop archive entries that no longer correspond to a live worktree.
 * Run at TUI startup to sweep ghosts left behind by destroys (which
 * intentionally don't touch archive.json — see `removeWorktree`) or by
 * external operations (`git worktree remove` from the shell). No-op
 * when nothing to drop, so the common case is a single read.
 */
export function reapArchived(liveSlugs: ReadonlySet<string>): void {
  const set = readArchived();
  let changed = false;
  for (const slug of set) {
    if (!liveSlugs.has(slug)) {
      set.delete(slug);
      changed = true;
    }
  }
  if (!changed) return;
  writeArchived(set);
}
