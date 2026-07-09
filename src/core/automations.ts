/**
 * Automation ledger + activity timestamps — the persistence half of the
 * automated-actions engine. The other half (condition evaluation over
 * row state and dispatch) lives in the TUI (`tui/automation-rules.ts`,
 * `tui/hooks/useAutomations.ts`) because it consumes `WorktreeRow`s;
 * this module owns everything that must survive a wt restart.
 *
 * # Fire keys and once-only semantics
 *
 * Triggers are LEVEL conditions ("checks are failing"), not edges — a
 * condition that held while wt was closed still gets handled once on
 * the next boot. Once-only comes from the ledger: every fire derives a
 * deterministic key (e.g. `ci:<slug>:<headSha>`) and a rule only fires
 * while its key is unseen. Key granularity controls re-firing: keying
 * on the head SHA means a new push that fails again re-triggers, the
 * same failure doesn't.
 *
 * # Two-phase entries
 *
 * `dispatched` is written SYNCHRONOUSLY before the dispatch path does
 * any async work (the one hard rule that makes concurrent evaluation
 * passes safe), then flipped to `delivered` once the launch resolves.
 * On boot, a stuck `dispatched` entry is reconciled against the
 * rehydrated action runs by fire key (headless runs stamp their keys
 * into meta.json): matched → delivered, unmatched → dropped so the
 * still-true condition re-fires. Session injections have no artifact
 * to reconcile against; their sub-second crash window is accepted.
 *
 * # Circuit breaker
 *
 * Guards the loop no key or cooldown can: auto-fix pushes a broken fix
 * → CI fails on the NEW sha → new fire key → new run. Per (rule, slug)
 * we count consecutive dispatches without ever observing the condition
 * clear in between; at `BREAKER_LIMIT` the rule trips for that worktree
 * and stays tripped until the condition is seen false (i.e. someone
 * actually fixed it). All state here so it survives restarts.
 */
import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

import { createLogger } from "./logger.ts";
import { WT_STATE_DIR } from "./wtstate.ts";

let ledgerFile = join(WT_STATE_DIR, "automations.json");
const log = createLogger("[automations]");

/** Consecutive no-clear dispatches per (rule, slug) before tripping. */
export const BREAKER_LIMIT = 2;

/** Fired entries older than this are pruned at load. */
const FIRED_RETENTION_MS = 30 * 24 * 60 * 60 * 1000;

export type FireState = "dispatched" | "delivered";

type FiredEntry = {
  state: FireState;
  at: number;
  ruleId: string;
  slug: string;
};

export type BreakerEntry = {
  /** Consecutive dispatches without the condition clearing in between. */
  count: number;
  /** Set when the breaker tripped; cleared when the condition clears. */
  trippedAt: number | null;
};

type Ledger = {
  version: 1;
  /** fireKey → entry. */
  fired: Record<string, FiredEntry>;
  /** `${ruleId}|${slug}` → breaker state. */
  breaker: Record<string, BreakerEntry>;
  /** `${ruleId}|${slug}` → last dispatch timestamp (cooldown input). */
  lastDispatch: Record<string, number>;
};

function emptyLedger(): Ledger {
  return { version: 1, fired: {}, breaker: {}, lastDispatch: {} };
}

let ledger: Ledger | null = null;

function pairKey(ruleId: string, slug: string): string {
  return `${ruleId}|${slug}`;
}

function loadLedger(): Ledger {
  if (ledger) return ledger;
  if (!existsSync(ledgerFile)) {
    ledger = emptyLedger();
    return ledger;
  }
  try {
    const raw = JSON.parse(readFileSync(ledgerFile, "utf8")) as Partial<Ledger>;
    const next = emptyLedger();
    const cutoff = Date.now() - FIRED_RETENTION_MS;
    if (raw?.fired && typeof raw.fired === "object") {
      for (const [k, v] of Object.entries(raw.fired)) {
        if (!v || typeof v !== "object") continue;
        const e = v as Partial<FiredEntry>;
        if (e.state !== "dispatched" && e.state !== "delivered") continue;
        if (typeof e.at !== "number" || e.at < cutoff) continue;
        next.fired[k] = {
          state: e.state,
          at: e.at,
          ruleId: typeof e.ruleId === "string" ? e.ruleId : "",
          slug: typeof e.slug === "string" ? e.slug : "",
        };
      }
    }
    if (raw?.breaker && typeof raw.breaker === "object") {
      for (const [k, v] of Object.entries(raw.breaker)) {
        if (!v || typeof v !== "object") continue;
        const e = v as Partial<BreakerEntry>;
        const count = typeof e.count === "number" && Number.isFinite(e.count) ? e.count : 0;
        const trippedAt = typeof e.trippedAt === "number" ? e.trippedAt : null;
        if (count <= 0 && trippedAt === null) continue;
        next.breaker[k] = { count, trippedAt };
      }
    }
    if (raw?.lastDispatch && typeof raw.lastDispatch === "object") {
      for (const [k, v] of Object.entries(raw.lastDispatch)) {
        if (typeof v === "number" && v >= cutoff) next.lastDispatch[k] = v;
      }
    }
    ledger = next;
  } catch (err) {
    log.warn("ledger read failed; starting empty", {
      file: ledgerFile,
      err: err instanceof Error ? err.message : String(err),
    });
    ledger = emptyLedger();
  }
  return ledger;
}

/**
 * Atomic write (tmp + rename), same pattern as wtstate. Mutations are
 * rare (one per fire / breaker transition), so sync writes are fine.
 * Single-writer by assumption — only one TUI runs the engine.
 */
function saveLedger(): void {
  const l = loadLedger();
  try {
    mkdirSync(dirname(ledgerFile), { recursive: true });
    const tmp = `${ledgerFile}.${process.pid}.tmp`;
    writeFileSync(tmp, `${JSON.stringify(l, null, 2)}\n`);
    renameSync(tmp, ledgerFile);
  } catch (err) {
    log.warn("ledger write failed", {
      file: ledgerFile,
      err: err instanceof Error ? err.message : String(err),
    });
  }
}

/** True when the key was already dispatched or delivered. */
export function hasHandledFire(key: string): boolean {
  return key in loadLedger().fired;
}

/**
 * Record dispatch SYNCHRONOUSLY (call before any await in the dispatch
 * path) so a concurrent evaluation pass can't double-fire, and stamp
 * the cooldown clock. Breaker accounting is separate (`bumpBreaker`) —
 * the caller decides trip-vs-launch before committing the dispatch.
 */
export function markFiresDispatched(
  keys: readonly string[],
  ruleId: string,
  slug: string,
): void {
  const l = loadLedger();
  const at = Date.now();
  for (const k of keys) l.fired[k] = { state: "dispatched", at, ruleId, slug };
  l.lastDispatch[pairKey(ruleId, slug)] = at;
  saveLedger();
}

/**
 * Un-consume keys whose dispatch was DECLINED by a contention guard
 * before anything ran (action already running for the slug, restack
 * mutex held by a manual `R`, …). Deleting the entries lets the
 * still-true condition re-derive an intent on the next pass — the
 * declined dispatch never happened, so it must not count as handled.
 * Distinct from a run that launched and failed, which stays delivered
 * (no auto-retry).
 */
export function dropFires(keys: readonly string[]): void {
  const l = loadLedger();
  let changed = false;
  for (const k of keys) {
    if (k in l.fired) {
      delete l.fired[k];
      changed = true;
    }
  }
  if (changed) saveLedger();
}

/** Flip keys to delivered once the launch handed off successfully. */
export function markFiresDelivered(keys: readonly string[]): void {
  const l = loadLedger();
  let changed = false;
  for (const k of keys) {
    const e = l.fired[k];
    if (e && e.state !== "delivered") {
      l.fired[k] = { ...e, state: "delivered" };
      changed = true;
    }
  }
  if (changed) saveLedger();
}

/**
 * Boot reconciliation for entries stuck in `dispatched` (wt died inside
 * the dispatch window). `hasRunForKey` should answer "does a rehydrated
 * action run carry this fire key in its meta" — matched entries were
 * really launched (flip to delivered); unmatched ones never made it
 * (drop, so the still-true condition can re-fire). Returns the number
 * of entries dropped for logging.
 */
export function reconcileDispatchedFires(
  hasRunForKey: (key: string) => boolean,
): number {
  const l = loadLedger();
  let dropped = 0;
  let changed = false;
  for (const [k, e] of Object.entries(l.fired)) {
    if (e.state !== "dispatched") continue;
    if (hasRunForKey(k)) {
      l.fired[k] = { ...e, state: "delivered" };
    } else {
      delete l.fired[k];
      dropped++;
    }
    changed = true;
  }
  if (changed) saveLedger();
  return dropped;
}

export function breakerState(ruleId: string, slug: string): BreakerEntry {
  return loadLedger().breaker[pairKey(ruleId, slug)] ?? { count: 0, trippedAt: null };
}

/** Increment the consecutive-dispatch count; returns the new count. */
export function bumpBreaker(ruleId: string, slug: string): number {
  const l = loadLedger();
  const key = pairKey(ruleId, slug);
  const prev = l.breaker[key] ?? { count: 0, trippedAt: null };
  const next = { ...prev, count: prev.count + 1 };
  l.breaker[key] = next;
  saveLedger();
  return next.count;
}

/** Open the breaker: no more dispatches for this (rule, slug) until reset. */
export function tripBreaker(ruleId: string, slug: string): void {
  const l = loadLedger();
  const key = pairKey(ruleId, slug);
  const prev = l.breaker[key] ?? { count: 0, trippedAt: null };
  l.breaker[key] = { ...prev, trippedAt: Date.now() };
  saveLedger();
}

/**
 * The condition was observed FALSE for this (rule, slug): the failure
 * actually cleared, so the consecutive count and any trip reset. No-op
 * (no write) when there's nothing to reset.
 */
export function resetBreaker(ruleId: string, slug: string): void {
  const l = loadLedger();
  const key = pairKey(ruleId, slug);
  if (!(key in l.breaker)) return;
  delete l.breaker[key];
  saveLedger();
}

/** Last dispatch timestamp for cooldown checks; null when never fired. */
export function lastDispatchAt(ruleId: string, slug: string): number | null {
  return loadLedger().lastDispatch[pairKey(ruleId, slug)] ?? null;
}

/**
 * Test-only: point the ledger at a scratch file and drop the in-memory
 * singleton so the next call re-reads it. Never call outside tests —
 * the default path is the real user ledger.
 */
export function __setLedgerPathForTests(path: string): void {
  ledgerFile = path;
  ledger = null;
}

// ---------- worktree activity timestamps ----------

/**
 * Last observed working-tree edit per slug, fed by the TUI runtime's
 * per-worktree fs watchers (`WorktreeWatchSet` callback). In-memory
 * only — the settle window is a behavior damper, not a correctness
 * mechanism, so losing it on restart just means a fresh window.
 */
const lastEdit = new Map<string, number>();

export function recordWorktreeEdit(slug: string): void {
  lastEdit.set(slug, Date.now());
}

/** 0 when no edit has been observed this session (treated as "long ago"). */
export function lastWorktreeEditAt(slug: string): number {
  return lastEdit.get(slug) ?? 0;
}
