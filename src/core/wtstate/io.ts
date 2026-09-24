import { dirname } from "node:path";

import { config } from "../config.ts";
import { withFileLock } from "../locks.ts";
import { createLogger } from "../logger.ts";
import { parseMergeEdge, type MergeEdge } from "../merge-edges.ts";
import { readRepositoryStateJson, writeRepositoryStateJson } from "../state-db.ts";
import { parseWorkStatus, sanitizeWorkNote } from "../work-status.ts";
import { migrateRawWtState, rawWtStateVersion, WT_STATE_VERSION } from "./migrations.ts";
import { GROUP_INBOX, STACK_SECTION_PREFIX } from "./types.ts";
import type {
  RemovedWorktree,
  ReviewRequestDismissal,
  WtSlugState,
  WtState,
} from "./types.ts";

/**
 * Directory holding the shared durable SQLite database. Exported so the
 * TUI watcher can observe writes made by another wt process.
 */
export const WT_STATE_DIR = dirname(config.paths.stateDb);
export const log = createLogger("[wtstate]");

export function readWtState(): WtState {
  const text = readRepositoryStateJson();
  if (text === null) return emptyWtState();
  try {
    return parseWtState(maybeMigrateRaw(JSON.parse(text)));
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { db: config.paths.stateDb });
    return emptyWtState();
  }
}

/**
 * The hot-path gate for every read: a single integer comparison once
 * the file is at the current version (the common case after the first
 * migrated write). Only version < WT_STATE_VERSION or > WT_STATE_VERSION
 * take a slower path; both are rare (one bump, or a rolled-back binary).
 */
function maybeMigrateRaw(parsed: unknown): Record<string, unknown> {
  const raw = (parsed && typeof parsed === "object" ? parsed : {}) as Record<string, unknown>;
  const from = rawWtStateVersion(raw);
  if (from === WT_STATE_VERSION) return raw;
  if (from > WT_STATE_VERSION) {
    // Newer-code state (rollback scenario): read leniently, never
    // rewrite or stamp — this build doesn't know the newer shape, and
    // `parseWtState` already drops fields it doesn't recognize.
    log.warn("repository state version is newer than this build supports; reading leniently", {
      db: config.paths.stateDb,
      fileVersion: from,
      buildVersion: WT_STATE_VERSION,
    });
    return raw;
  }
  // from < WT_STATE_VERSION: migrate and persist so this cost is paid
  // once. `readWtState` can itself be called from inside a mutator's
  // `withWtStateLock` critical section (every mutator in sections.ts /
  // removed.ts / automations-pause.ts does `readWtState()` while
  // holding the lock) — flock is per-open-file-description, not
  // reentrant within a process, so re-acquiring here would deadlock.
  // `wtStateLockDepth` tracks whether we're already inside one.
  const persist = (): Record<string, unknown> => migrateAndPersist(raw);
  return wtStateLockDepth > 0 ? persist() : withWtStateLock(persist);
}

/**
 * Runs INSIDE the wtstate lock (held by the caller either way — see
 * `maybeMigrateRaw`). Re-reads the row fresh: another process may have
 * already migrated it while this one waited for the lock, in which case
 * this is a cheap no-op. Runs the existing forward-only payload migration
 * chain and persists it as one SQLite row update.
 */
function migrateAndPersist(fallback: Record<string, unknown>): Record<string, unknown> {
  // Re-read after acquiring the repository lock: another process may have
  // migrated or mutated this row while this reader was waiting. Persisting
  // the pre-lock snapshot would turn a safe schema upgrade into a lost write.
  let current = fallback;
  const fresh = readRepositoryStateJson();
  if (fresh !== null) {
    try {
      current = JSON.parse(fresh) as Record<string, unknown>;
    } catch {
      // The caller already parsed a valid snapshot; retain it if a manually
      // corrupted concurrent value appeared between the two reads.
    }
  }
  const from = rawWtStateVersion(current);
  if (from >= WT_STATE_VERSION) return current;
  const { value } = migrateRawWtState(current);
  try {
    writeRepositoryStateJson(JSON.stringify(value));
  } catch (err) {
    // Non-fatal: the migrated value is still used in-memory for this
    // read. A failed persist just means the next read repeats this
    // (cheap, idempotent) migration instead of hitting the fast path.
    log.error(err instanceof Error ? err : String(err), { db: config.paths.stateDb, phase: "migrate-persist" });
  }
  log.info("migrated repository state", { db: config.paths.stateDb, repoId: config.repoId, from, to: WT_STATE_VERSION });
  return value;
}

/**
 * Pure validation/coercion from parsed JSON to `WtState`, split out of
 * `readWtState` so the field-by-field tolerance rules (unknown shapes
 * degrade to defaults rather than throwing) are unit-testable without
 * touching the real state database. Never throws:
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
      if (typeof rec.createdAt === "string" && Number.isFinite(Date.parse(rec.createdAt))) {
        slugs[k]!.createdAt = rec.createdAt;
      }
      if (typeof rec.baseBranch === "string" && rec.baseBranch.trim() !== "") {
        slugs[k]!.baseBranch = rec.baseBranch;
        if (typeof rec.baseSha === "string" && rec.baseSha.trim() !== "") {
          slugs[k]!.baseSha = rec.baseSha;
        }
      }
      if (
        typeof rec.devPort === "number" &&
        Number.isInteger(rec.devPort) &&
        rec.devPort > 0 &&
        rec.devPort <= 65_535
      ) {
        slugs[k]!.devPort = rec.devPort;
      }
      if (typeof rec.devStartedSha === "string" && rec.devStartedSha.trim() !== "") {
        slugs[k]!.devStartedSha = rec.devStartedSha.trim();
      }
      const ex = rec.examined;
      if (
        ex &&
        typeof ex === "object" &&
        typeof (ex as { sha?: unknown }).sha === "string" &&
        typeof (ex as { verdict?: unknown }).verdict === "string" &&
        typeof (ex as { at?: unknown }).at === "string"
      ) {
        const e = ex as { sha: string; verdict: string; at: string; by?: unknown };
        const verdict = sanitizeWorkNote(e.verdict);
        if (e.sha.trim() !== "" && verdict !== "") {
          const baseSha = (e as { baseSha?: unknown }).baseSha;
          slugs[k]!.examined = {
            sha: e.sha.trim(),
            verdict,
            at: e.at,
            ...(typeof baseSha === "string" && baseSha.trim() !== ""
              ? { baseSha: baseSha.trim() }
              : {}),
            ...(typeof e.by === "string" && e.by.trim() !== "" ? { by: e.by.trim() } : {}),
          };
        }
      }
      if (
        typeof rec.githubIssue === "number" &&
        Number.isInteger(rec.githubIssue) &&
        rec.githubIssue > 0
      ) {
        slugs[k]!.githubIssue = rec.githubIssue;
      }
      // Whitelisted like every other field on the entry: a value
      // written but not parsed round-trips to nothing with every test
      // still green (the `RemovedWorktree.work` lesson).
      // The EMPTY string round-trips deliberately: it is the third
      // state, "this worktree has no tracker issue", which absence
      // cannot express because absence means "fall back to the slug".
      // Dropping it here would silently turn an asserted none back
      // into whatever the slug happens to parse to.
      if (typeof rec.issueId === "string") {
        slugs[k]!.issueId = rec.issueId.trim().toUpperCase();
      }
      if (rec.automationsPaused === true) {
        slugs[k]!.automationsPaused = true;
      }
      const work = parseWorkStatus(rec.work);
      if (work) slugs[k]!.work = work;
    }
  }
  const remoteLayouts: WtState["remoteLayouts"] = {};
  if (data?.remoteLayouts && typeof data.remoteLayouts === "object") {
    for (const [key, value] of Object.entries(data.remoteLayouts)) {
      if (!value || typeof value !== "object") continue;
      const rec = value as { section?: unknown; order?: unknown };
      remoteLayouts[key] = {
        section:
          typeof rec.section === "string" && rec.section.trim() !== ""
            ? rec.section
            : null,
        order:
          typeof rec.order === "number" && Number.isFinite(rec.order)
            ? rec.order
            : 0,
      };
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
  for (const v of [...Object.values(slugs), ...Object.values(remoteLayouts)]) {
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
      // Same normalizer the live per-slug record uses, so a status
      // cannot mean one thing on a row and another in the history.
      const work = parseWorkStatus(rec.work);
      removed.push({
        slug: rec.slug,
        branch: rec.branch,
        removedAt: typeof rec.removedAt === "string" ? rec.removedAt : "",
        ...(typeof rec.title === "string" && rec.title.trim() !== "" ? { title: rec.title } : {}),
        ...(typeof rec.issueId === "string" ? { issueId: rec.issueId } : {}),
        ...(typeof rec.githubIssue === "number" && Number.isInteger(rec.githubIssue) && rec.githubIssue > 0
          ? { githubIssue: rec.githubIssue }
          : {}),
        ...(rec.gitState === "merged" || rec.gitState === "gone" ? { gitState: rec.gitState } : {}),
        ...(typeof rec.prNumber === "number" && Number.isFinite(rec.prNumber) ? { prNumber: rec.prNumber } : {}),
        ...(typeof rec.prUrl === "string" && rec.prUrl.trim() !== "" ? { prUrl: rec.prUrl } : {}),
        ...(typeof rec.prState === "string" && rec.prState.trim() !== "" ? { prState: rec.prState } : {}),
        ...(work ? { work } : {}),
        ...(rec.automationsPaused === true ? { automationsPaused: true } : {}),
      });
    }
  }
  const reviewRequestDismissals: ReviewRequestDismissal[] = [];
  if (Array.isArray(data?.reviewRequestDismissals)) {
    for (const value of data.reviewRequestDismissals) {
      if (!value || typeof value !== "object") continue;
      const rec = value as Partial<ReviewRequestDismissal>;
      if (
        typeof rec.url !== "string" || rec.url.trim() === "" ||
        typeof rec.updatedAt !== "string" || rec.updatedAt.trim() === "" ||
        typeof rec.dismissedAt !== "string" || rec.dismissedAt.trim() === ""
      ) continue;
      reviewRequestDismissals.push({
        url: rec.url.trim(),
        updatedAt: rec.updatedAt,
        dismissedAt: rec.dismissedAt,
      });
    }
  }
  // Branch → last observed tip. String values only; a malformed entry
  // drops to "not seen yet", which costs one skipped range rather than
  // a fire against a sha nothing can resolve.
  const branchTips: Record<string, string> = {};
  const rawTips = data?.branchTips;
  if (rawTips && typeof rawTips === "object" && !Array.isArray(rawTips)) {
    for (const [k, v] of Object.entries(rawTips as Record<string, unknown>)) {
      if (typeof v === "string" && v.trim() !== "") branchTips[k] = v;
    }
  }
  const edges: MergeEdge[] = [];
  if (Array.isArray(data?.edges)) {
    for (const v of data.edges) {
      const edge = parseMergeEdge(v);
      if (edge) edges.push(edge);
    }
  }
  return {
    version: WT_STATE_VERSION,
    slugs,
    remoteLayouts,
    sectionsOrder,
    foldedSections,
    pausedStacks,
    automationsPaused: data?.automationsPaused === true,
    attentionSeenTs:
      typeof data?.attentionSeenTs === "number" &&
      Number.isFinite(data.attentionSeenTs) &&
      data.attentionSeenTs > 0
        ? data.attentionSeenTs
        : 0,
    removed,
    reviewRequestDismissals,
    edges,
    branchTips,
  };
}

export function emptyWtState(): WtState {
  return {
    version: WT_STATE_VERSION,
    slugs: {},
    remoteLayouts: {},
    sectionsOrder: [],
    foldedSections: [],
    pausedStacks: [],
    automationsPaused: false,
    attentionSeenTs: 0,
    removed: [],
    reviewRequestDismissals: [],
    edges: [],
    branchTips: {},
  };
}

export function writeWtState(state: WtState): void {
  try {
    // Every write path stamps the CURRENT version, matching
    // `parseWtState`'s tolerate-unknown-fields contract: a value read
    // leniently from a newer-code file already had its not-yet-known
    // fields dropped at parse time, so there's no newer shape left to
    // preserve by keeping a higher version number here.
    writeRepositoryStateJson(JSON.stringify({ ...state, version: WT_STATE_VERSION }));
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { db: config.paths.stateDb });
    // Re-raise so the action layer can surface the failure to the
    // user (toast + event log). Silently swallowing here would
    // present a successful move while the state file is unchanged.
    throw err;
  }
}

/**
 * Reentrancy depth for the wtstate flock. `flock` locks an open file
 * description, not a process — a second `open()` + `flock()` against
 * the same path from the SAME process still blocks (there's no thread
 * to unblock it), so a naive nested `withWtStateLock` call would
 * deadlock. Every mutator in sections.ts/removed.ts/automations-pause.ts
 * calls `readWtState()` while already holding this lock; the migration
 * path in `readWtState` needs to write too, so it checks this depth to
 * decide whether to acquire the lock itself or just run inline under
 * the caller's held lock.
 */
let wtStateLockDepth = 0;

/**
 * Serialize a repository-state read-modify-write across processes. SQLite
 * stops torn writes, but two concurrent read/modify/write callers
 * (the TUI's startup reap vs a CLI `wt base` mutation) each write back
 * from their own pre-write snapshot, silently dropping whichever update
 * landed in between. Every mutator below wraps its read→mutate→write in
 * this blocking flock; the critical sections are pure sync JSON work, so
 * the kernel wait is sub-millisecond and crash-safe (fd close releases).
 */
export function withWtStateLock<T>(fn: () => T): T {
  return withFileLock("__wtstate__", () => {
    wtStateLockDepth++;
    try {
      return fn();
    } finally {
      wtStateLockDepth--;
    }
  });
}
