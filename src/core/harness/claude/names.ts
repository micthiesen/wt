/**
 * Per-slug list of named claude sessions the user has spawned. Primary
 * (F12) is implicit — never stored here. Backing file:
 * `~/.cache/wt/claude-sessions.json`.
 *
 * The exported helpers cover both the persistence layer (read / mutate
 * the JSON file) and the picker-data composition
 * (`buildClaudeSessionEntries`, which composes the rich entry shape
 * the keyboard handler and the JSX render both consume). Keeping the
 * latter here means the two callers can't silently drift in the
 * picker's item ordering.
 *
 * Names alone are persisted; UUIDs are derived deterministically from
 * `(wtPath, name)` via `wtSessionUuid`, so a wt restart (or a
 * reboot-killed tmux server) reconstructs the full picture from the
 * names list plus the path. That's what lets the picker resume a
 * session whose tmux session is dead but whose conversation jsonl is
 * still on disk.
 *
 * Names are validated: `^[a-zA-Z0-9_-]+$`, no `~` (the tmux session-name
 * separator). The picker reserves `primary` and digits used by
 * `nextAutoName` so the auto-numbering ladder stays predictable.
 */
import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

import { wtSessionUuid, type SessionTail } from "./jsonl.ts";
import type { RegistryStatus } from "./registry.ts";
import { deriveSessionState, type DerivedState } from "../status.ts";
import type { SessionSummary } from "./summaries.ts";
import { withFileLock } from "../../locks.ts";
import { createLogger } from "../../logger.ts";

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
  if (trimmed.toLowerCase() === "primary") return "`primary` is reserved";
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
    // Write-then-rename so a concurrent reader never sees a torn file.
    const tmp = `${STATE_FILE}.${process.pid}.tmp`;
    writeFileSync(tmp, `${JSON.stringify(shape, null, 2)}\n`);
    renameSync(tmp, STATE_FILE);
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { file: STATE_FILE });
    throw err;
  }
}

/**
 * Serialize the read-modify-write across processes — same rationale as
 * `withWtStateLock` in wtstate.ts: the atomic rename stops torn reads,
 * the flock stops two writers dropping each other's update.
 */
function withNamesLock<T>(fn: () => T): T {
  return withFileLock("__claude_names__", fn);
}

/** Names registered for `slug`, in insertion order. Empty when none. */
export function listClaudeNames(slug: string): string[] {
  const shape = readFile();
  return shape[slug] ? [...shape[slug]] : [];
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
  withNamesLock(() => {
    const shape = readFile();
    const existing = shape[slug] ?? [];
    if (existing.includes(name)) return;
    shape[slug] = [...existing, name];
    writeFile(shape);
  });
}

/** Drop one name. No-op when absent. */
export function removeClaudeName(slug: string, name: string): void {
  withNamesLock(() => {
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
  });
}

/** Drop every name for `slug`. Called from worktree-destroy. */
export function clearClaudeNames(slug: string): void {
  withNamesLock(() => {
    const shape = readFile();
    if (!(slug in shape)) return;
    delete shape[slug];
    writeFile(shape);
  });
}

/**
 * Smallest positive integer N such that `${N}` isn't in `slug`'s name
 * list. Default for the `+ new` row when the user hits enter on an
 * empty input. Starts at 2 so the auto-numbered ladder reads as
 * "primary, 2, 3, ..." even though primary isn't stored here.
 */
export function nextAutoName(slug: string): string {
  const taken = new Set(listClaudeNames(slug));
  let n = 2;
  while (taken.has(`${n}`)) n++;
  return `${n}`;
}

/**
 * One row's worth of state for the sessions picker. Rich entries
 * carry everything the picker needs to render and sort:
 *   - identity (`name`, `sessionId`)
 *   - liveness (`isLive` — tmux-managed by wt; the registry covers
 *     non-wt processes too but they aren't pickable)
 *   - derived state (`working / waiting / abandoned / idle`)
 *   - last-activity timestamp (`lastEntryMs`) for the age column
 *   - typed-ahead `queued` count
 *   - LLM-authored `summary` snippet (or null when nothing yet)
 *
 * The trailing "+ new" affordance is appended downstream by the
 * picker UI itself; this helper returns only the session entries.
 */
export type ClaudeSessionPickerEntry = {
  name: string | null;
  /**
   * Deterministic UUID derived from (wtPath, name). Matches the
   * sessionId in the registry and the jsonl filename, so consumers
   * can look up summary / registry status without re-deriving.
   */
  sessionId: string;
  isLive: boolean;
  state: DerivedState;
  /** Most-recent meaningful jsonl entry timestamp, or null when empty. */
  lastEntryMs: number | null;
  queued: number;
  summary: SessionSummary | null;
};

/**
 * Picker sort order — a *to-do list*, not a severity ranking: the user
 * is most likely to want to attach to a session that's blocked on them
 * (`asking`), then one that's ready for input (`waiting`), over one
 * that crashed mid-turn (`abandoned`, which needs investigation).
 */
const STATE_RANK: Record<DerivedState, number> = {
  asking: 0,
  working: 1,
  polling: 2,
  unknown: 3,
  waiting: 4,
  abandoned: 5,
  idle: 6,
};

/**
 * Build the picker's session entries for one worktree. Order is
 * status-priority-first (working > waiting > abandoned > idle), then
 * within each bucket primary before named, then by most-recent
 * activity first. Single source of truth so the keyboard handler and
 * the JSX render never drift.
 *
 * Caller passes the full lookup bag — keeping pure inputs lets the
 * picker recompute synchronously on every render-driving signal
 * change (registry flip, tmux churn, new summary fetch) without
 * paying for the listClaudeNames disk read twice.
 *
 * `liveNames` — names live in tmux right now (`null` = primary).
 * `tailByName` — jsonl tails keyed by `name | null` for the worktree.
 * `registryStatusBySessionId` — busy/idle per sessionId from
 *   `~/.claude/sessions/<pid>.json`. Look up via
 *   `wtSessionUuid(wt.path, name)`.
 * `summaryBySessionId` — per-session summary snippet (or null).
 */
export function buildClaudeSessionEntries(opts: {
  slug: string;
  wtPath: string;
  liveNames: ReadonlyArray<string | null>;
  tailByName: ReadonlyMap<string | null, SessionTail>;
  registryStatusBySessionId: Readonly<Record<string, RegistryStatus>>;
  summaryBySessionId: Readonly<Record<string, SessionSummary | null>>;
}): ClaudeSessionPickerEntry[] {
  const { slug, wtPath, liveNames, tailByName, registryStatusBySessionId, summaryBySessionId } = opts;
  const liveSet = new Set(liveNames);
  const persisted = listClaudeNames(slug);
  // Names we care about: primary (always) + every persisted name +
  // any tmux-live name that somehow isn't persisted (defensive).
  const seen = new Set<string>();
  const namedNames: string[] = [];
  for (const n of persisted) {
    if (!seen.has(n)) {
      seen.add(n);
      namedNames.push(n);
    }
  }
  for (const n of liveNames) {
    if (n === null || seen.has(n)) continue;
    seen.add(n);
    namedNames.push(n);
  }
  const allNames: ReadonlyArray<string | null> = [null, ...namedNames];

  const out: ClaudeSessionPickerEntry[] = allNames.map((name) => {
    const sessionId = wtSessionUuid(wtPath, name);
    const tail =
      tailByName.get(name) ??
      ({
        name,
        hasJsonl: false,
        lastEntryMs: null,
        lastEntryKind: null,
        queued: 0,
        pendingAsk: null,
        lastAssistantText: null,
      } satisfies SessionTail);
    const regStatus = registryStatusBySessionId[sessionId] ?? null;
    const isLive = liveSet.has(name);
    return {
      name,
      sessionId,
      isLive,
      state: deriveSessionState(tail, isLive, regStatus),
      lastEntryMs: tail.lastEntryMs,
      queued: tail.queued,
      summary: summaryBySessionId[sessionId] ?? null,
    };
  });

  // Filter ghost entries that have no live tmux AND no on-disk
  // signal at all (no jsonl, no registry process). Those are stale
  // persisted names left behind by spawn-failed entries; surfacing
  // them in the picker invites confusion.
  const filtered = out.filter((e) => {
    if (e.isLive) return true;
    if (e.state !== "idle") return true;
    if (e.lastEntryMs !== null) return true;
    if (e.name === null) return true; // always show primary
    return false;
  });

  filtered.sort((a, b) => {
    const rankDiff = STATE_RANK[a.state] - STATE_RANK[b.state];
    if (rankDiff !== 0) return rankDiff;
    // Within a state bucket: primary first, then by recency desc.
    if ((a.name === null) !== (b.name === null)) return a.name === null ? -1 : 1;
    const aMs = a.lastEntryMs ?? 0;
    const bMs = b.lastEntryMs ?? 0;
    if (aMs !== bMs) return bMs - aMs;
    return (a.name ?? "").localeCompare(b.name ?? "");
  });
  return filtered;
}

/**
 * Reap stale slug entries against the live slug set. Called after
 * destroys to keep the state file tidy. No-op when nothing to drop.
 */
export function reapClaudeNames(liveSlugs: ReadonlySet<string>): void {
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
