/**
 * Per-slug list of named claude sessions the user has spawned. Primary
 * (F12) is implicit — never stored here. Backing file:
 * `~/.cache/wt/claude-sessions.json`.
 *
 * Names alone are persisted; UUIDs are derived deterministically from
 * `(wtPath, name)` via `wtSessionUuid`, so a wt restart (or a
 * reboot-killed tmux server) reconstructs the full picture from the
 * names list plus the path. This is what makes "ghost" sessions
 * (tmux dead, conversation alive on disk) attachable in one keystroke.
 *
 * Names are validated: `^[a-zA-Z0-9_-]+$`, no `~` (the tmux session-name
 * separator). The picker reserves `primary` and digits used by
 * `nextAutoNumber` so the auto-numbering ladder stays predictable.
 */
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

import { createLogger } from "./logger.ts";

const STATE_FILE = join(homedir(), ".cache", "wt", "claude-sessions.json");
const log = createLogger("[claude-sessions]");

/** Raw file shape: `{ <slug>: ["name1", "name2", ...] }`. */
type FileShape = Record<string, string[]>;

const NAME_RE = /^[a-zA-Z0-9_-]+$/;

/**
 * Validate a user-supplied session name. Returns null when accepted,
 * an error string when rejected — caller surfaces via toast.
 */
export function validateSessionName(input: string): string | null {
  const trimmed = input.trim();
  if (!trimmed) return "name can't be empty";
  if (trimmed.length > 32) return "name too long (max 32)";
  if (trimmed === "primary") return "`primary` is reserved";
  if (!NAME_RE.test(trimmed)) {
    return "use letters, digits, _ or - only";
  }
  return null;
}

function readFile(): FileShape {
  if (!existsSync(STATE_FILE)) return {};
  try {
    const raw = readFileSync(STATE_FILE, "utf8");
    const parsed = JSON.parse(raw) as unknown;
    if (!parsed || typeof parsed !== "object") return {};
    const out: FileShape = {};
    for (const [slug, names] of Object.entries(parsed as Record<string, unknown>)) {
      if (!Array.isArray(names)) continue;
      const seen = new Set<string>();
      const list: string[] = [];
      for (const n of names) {
        if (typeof n !== "string") continue;
        if (validateSessionName(n) !== null) continue;
        if (seen.has(n)) continue;
        seen.add(n);
        list.push(n);
      }
      out[slug] = list;
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
    writeFileSync(STATE_FILE, `${JSON.stringify(shape, null, 2)}\n`);
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { file: STATE_FILE });
    throw err;
  }
}

/** Names registered for `slug`, in insertion order. Empty when none. */
export function listClaudeNames(slug: string): string[] {
  const shape = readFile();
  return shape[slug] ? [...shape[slug]] : [];
}

/** All slugs that have at least one named session on file. */
export function allSlugsWithNamedSessions(): string[] {
  return Object.keys(readFile());
}

/** Whether `name` already exists for `slug`. Primary is never matched. */
export function nameInUse(slug: string, name: string): boolean {
  return listClaudeNames(slug).includes(name);
}

/**
 * Append a name to `slug`'s list. No-op when already present (caller
 * checks `nameInUse` if it wants to differentiate resume vs. fresh).
 */
export function addClaudeName(slug: string, name: string): void {
  const shape = readFile();
  const existing = shape[slug] ?? [];
  if (existing.includes(name)) return;
  shape[slug] = [...existing, name];
  writeFile(shape);
}

/** Drop one name. No-op when absent. */
export function removeClaudeName(slug: string, name: string): void {
  const shape = readFile();
  const existing = shape[slug];
  if (!existing) return;
  const next = existing.filter((n) => n !== name);
  if (next.length === existing.length) return;
  if (next.length === 0) {
    delete shape[slug];
  } else {
    shape[slug] = next;
  }
  writeFile(shape);
}

/** Drop every name for `slug`. Called from worktree-destroy. */
export function clearClaudeNames(slug: string): void {
  const shape = readFile();
  if (!(slug in shape)) return;
  delete shape[slug];
  writeFile(shape);
}

/**
 * Smallest positive integer N such that `${N}` isn't in `slug`'s name
 * list. Default for the `+ new` row when the user hits enter on an
 * empty input. Starts at 2 so the auto-numbered ladder reads as
 * "primary, 2, 3, ..." even though primary isn't stored here.
 */
export function nextAutoNumber(slug: string): string {
  const taken = new Set(listClaudeNames(slug));
  let n = 2;
  while (taken.has(`${n}`)) n++;
  return `${n}`;
}

/**
 * Reap stale slug entries against the live slug set. Called after
 * destroys to keep the state file tidy. No-op when nothing to drop.
 */
export function reapClaudeNames(liveSlugs: ReadonlySet<string>): void {
  const shape = readFile();
  let changed = false;
  for (const slug of Object.keys(shape)) {
    if (!liveSlugs.has(slug)) {
      delete shape[slug];
      changed = true;
    }
  }
  if (changed) writeFile(shape);
}
