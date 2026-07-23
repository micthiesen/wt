import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

import { withFileLock } from "../locks.ts";
import { createLogger } from "../logger.ts";
import { GROUP_INBOX, STACK_SECTION_PREFIX } from "./types.ts";
import type { RemovedWorktree, WtSlugState, WtState } from "./types.ts";

/**
 * Directory holding the cross-process state files (`state.json` here,
 * `archive.json` in archive.ts). Exported so the TUI's state-file
 * watcher (`watchWtStateFiles` in repo-watch.ts) observes the same
 * location these writers target.
 */
export const WT_STATE_DIR = join(homedir(), ".cache", "wt");
export const STATE_FILE = join(WT_STATE_DIR, "state.json");
export const log = createLogger("[wtstate]");

export function readWtState(): WtState {
  if (!existsSync(STATE_FILE)) return emptyWtState();
  try {
    const raw = readFileSync(STATE_FILE, "utf8");
    return parseWtState(JSON.parse(raw));
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { file: STATE_FILE });
    return emptyWtState();
  }
}

/**
 * Pure validation/coercion from parsed JSON to `WtState`, split out of
 * `readWtState` so the field-by-field tolerance rules (unknown shapes
 * degrade to defaults rather than throwing) are unit-testable without
 * touching the real state file — `readWtState` hardcodes `STATE_FILE`
 * to `~/.cache/wt/state.json` with no injection seam. Never throws:
 * callers that already have a parsed JSON value (not raw text) can use
 * this directly instead of round-tripping through `readWtState`.
 */
export function parseWtState(raw: unknown): WtState {
  const data = raw as Partial<WtState>;
  const slugs: Record<string, WtSlugState> = {};
  if (data?.slugs && typeof data.slugs === "object") {
    for (const [k, v] of Object.entries(data.slugs)) {
      if (!v || typeof v !== "object") continue;
      const rec = v as Partial<WtSlugState>;
      const section = typeof rec.section === "string" && rec.section.trim() !== ""
        ? rec.section
        : null;
      const order = typeof rec.order === "number" && Number.isFinite(rec.order) ? rec.order : 0;
      slugs[k] = { section, order };
      if (typeof rec.baseBranch === "string" && rec.baseBranch.trim() !== "") {
        slugs[k]!.baseBranch = rec.baseBranch;
        if (typeof rec.baseSha === "string" && rec.baseSha.trim() !== "") {
          slugs[k]!.baseSha = rec.baseSha;
        }
      }
      if (rec.automationsPaused === true) {
        slugs[k]!.automationsPaused = true;
      }
      if (rec.taskPinned === true) {
        slugs[k]!.taskPinned = true;
      }
      if (typeof rec.taskSnoozedBucket === "string" && rec.taskSnoozedBucket.trim() !== "") {
        slugs[k]!.taskSnoozedBucket = rec.taskSnoozedBucket;
      }
    }
  }
  const rawOrder: string[] = [];
  if (Array.isArray(data?.sectionsOrder)) {
    const seen = new Set<string>();
    for (const s of data.sectionsOrder) {
      if (typeof s !== "string" || s.trim() === "") continue;
      if (seen.has(s)) continue;
      seen.add(s);
      rawOrder.push(s);
    }
  }
  let sectionsOrder: string[];
  if (!rawOrder.includes(GROUP_INBOX)) {
    // Pre-unification file (manual names only): seed the unified order
    // with the legacy bucket layout so the migration changes nothing
    // visually — the inbox, then the manual sections in their stored
    // order. Stack keys (inferred at runtime) enter lazily on a move.
    sectionsOrder = [
      GROUP_INBOX,
      ...rawOrder.filter((s) => !s.startsWith(STACK_SECTION_PREFIX)),
    ];
  } else {
    // Stack liveness can't be checked here (stacks are inferred from
    // the live worktree list, which this module doesn't see); stale
    // stack keys are inert — nothing renders for them — and cheap.
    sectionsOrder = rawOrder;
  }
  // Self-heal: any manual section referenced by a slug but missing from
  // sectionsOrder gets appended in discovery order.
  const known = new Set(sectionsOrder);
  for (const v of Object.values(slugs)) {
    if (v.section !== null && !known.has(v.section)) {
      sectionsOrder.push(v.section);
      known.add(v.section);
    }
  }
  const foldedSections: string[] = [];
  if (Array.isArray(data?.foldedSections)) {
    const seen = new Set<string>();
    for (const s of data.foldedSections) {
      if (typeof s !== "string" || s.trim() === "" || seen.has(s)) continue;
      seen.add(s);
      foldedSections.push(s);
    }
  }
  const pausedStacks: string[] = [];
  if (Array.isArray(data?.pausedStacks)) {
    const seen = new Set<string>();
    for (const s of data.pausedStacks) {
      if (typeof s !== "string" || s.trim() === "" || seen.has(s)) continue;
      seen.add(s);
      pausedStacks.push(s);
    }
  }
  const removed: RemovedWorktree[] = [];
  if (Array.isArray(data?.removed)) {
    for (const v of data.removed) {
      if (!v || typeof v !== "object") continue;
      const rec = v as Partial<RemovedWorktree>;
      if (typeof rec.slug !== "string" || rec.slug.trim() === "") continue;
      if (typeof rec.branch !== "string" || rec.branch.trim() === "") continue;
      removed.push({
        slug: rec.slug,
        branch: rec.branch,
        removedAt: typeof rec.removedAt === "string" ? rec.removedAt : "",
        ...(typeof rec.title === "string" && rec.title.trim() !== "" ? { title: rec.title } : {}),
        ...(typeof rec.prNumber === "number" && Number.isFinite(rec.prNumber) ? { prNumber: rec.prNumber } : {}),
        ...(typeof rec.prUrl === "string" && rec.prUrl.trim() !== "" ? { prUrl: rec.prUrl } : {}),
        ...(typeof rec.prState === "string" && rec.prState.trim() !== "" ? { prState: rec.prState } : {}),
      });
    }
  }
  return {
    slugs,
    sectionsOrder,
    foldedSections,
    pausedStacks,
    automationsPaused: data?.automationsPaused === true,
    removed,
  };
}

export function emptyWtState(): WtState {
  return {
    slugs: {},
    sectionsOrder: [],
    foldedSections: [],
    pausedStacks: [],
    automationsPaused: false,
    removed: [],
  };
}

export function writeWtState(state: WtState): void {
  try {
    mkdirSync(dirname(STATE_FILE), { recursive: true });
    // Write-then-rename so a concurrent reader (the live TUI polls this
    // file) never observes a half-written file and silently falls back
    // to empty defaults. rename(2) is atomic within a filesystem. This
    // closes the torn-read window; lost updates between two WRITERS are
    // closed separately by `withWtStateLock` spanning each mutator's
    // read-modify-write.
    const tmp = `${STATE_FILE}.${process.pid}.tmp`;
    writeFileSync(tmp, `${JSON.stringify(state, null, 2)}\n`);
    renameSync(tmp, STATE_FILE);
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { file: STATE_FILE });
    // Re-raise so the action layer can surface the failure to the
    // user (toast + event log). Silently swallowing here would
    // present a successful move while the state file is unchanged.
    throw err;
  }
}

/**
 * Serialize a state-file read-modify-write across processes. The atomic
 * rename in `writeWtState` stops torn reads, but two concurrent WRITERS
 * (the TUI's startup reap vs a CLI `wt base` mutation) each write back
 * from their own pre-write snapshot, silently dropping whichever update
 * landed in between. Every mutator below wraps its read→mutate→write in
 * this blocking flock; the critical sections are pure sync JSON work, so
 * the kernel wait is sub-millisecond and crash-safe (fd close releases).
 */
export function withWtStateLock<T>(fn: () => T): T {
  return withFileLock("__wtstate__", fn);
}
