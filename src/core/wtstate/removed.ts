import { createLogger } from "../logger.ts";
import { readWtState, withWtStateLock, writeWtState } from "./io.ts";
import type { RemovedWorktree } from "./types.ts";

/** Bounds on the removed-worktrees history, enforced at write time. */
const REMOVED_MAX_ENTRIES = 30;
const REMOVED_MAX_AGE_MS = 14 * 24 * 60 * 60 * 1000;

/**
 * Window within which a destroyed worktree still shows on fleet
 * surfaces (`wt ls`, `wt status --all --json`, the empty-state
 * messages) as "recently merged/archived". Long enough that a manager
 * scanning once a day still sees what landed; short enough that the
 * section stays news, not history (`h` keeps the full 14-day record).
 */
export const RECENT_REMOVED_WINDOW_MS = 48 * 60 * 60 * 1000;

/** True when either the PR or the checkout's git state proved it landed. */
export function isMergedRemoval(e: RemovedWorktree): boolean {
  return e.prState === "MERGED" || e.gitState === "merged";
}

/**
 * Removed-history entries inside the recent window whose slug isn't
 * live again, newest first. This is the derivation fleet surfaces use
 * to distinguish "everything merged" from "no worktrees exist" — no
 * new store, just a view over the existing removed history.
 */
export function recentlyRemovedWorktrees(
  liveSlugs: ReadonlySet<string>,
  nowMs: number = Date.now(),
): RemovedWorktree[] {
  const cutoff = nowMs - RECENT_REMOVED_WINDOW_MS;
  return readWtState()
    .removed.filter(
      (e) => !liveSlugs.has(e.slug) && (Date.parse(e.removedAt) || 0) >= cutoff,
    )
    .sort((a, b) => b.removedAt.localeCompare(a.removedAt));
}

/**
 * One-line summary of recent removals for empty states ("2 archived
 * today: eng-1, eng-2"). Null when there's nothing recent — callers
 * fall back to their plain empty message.
 */
export function recentRemovalsSummary(
  entries: readonly RemovedWorktree[],
  nowMs: number = Date.now(),
): string | null {
  if (entries.length === 0) return null;
  const dayCutoff = nowMs - 24 * 60 * 60 * 1000;
  const today = entries.every((e) => (Date.parse(e.removedAt) || 0) >= dayCutoff);
  const named = entries.slice(0, 4).map((e) => e.slug);
  const more = entries.length - named.length;
  const list = named.join(", ") + (more > 0 ? `, +${more} more` : "");
  return `${entries.length} archived ${today ? "today" : "in the last 2 days"}: ${list}`;
}

/**
 * Record destroyed worktrees into the removed history. Upserts by slug:
 * defined fields of the incoming entry win, existing rich fields (title,
 * PR snapshot) survive a later minimal write — so the TUI's dispatch-time
 * snapshot and `removeWorktree`'s on-success confirmation compose in
 * either order. Prunes by age and caps the list, newest first.
 */
export function recordRemovedWorktrees(
  entries: readonly RemovedWorktree[],
): void {
  if (entries.length === 0) return;
  withWtStateLock(() => {
    const state = readWtState();
    const bySlug = new Map(state.removed.map((e) => [e.slug, e]));
    for (const e of entries) {
      const prev = bySlug.get(e.slug);
      // The work status is read HERE rather than threaded in by the
      // caller: this function already holds the whole state under the
      // lock, so the knowledge is first-hand, and a removal path added
      // later cannot forget to pass it. `reapWtState` drops the
      // per-slug record along with the worktree, so this copy is the
      // only place the answer survives. Explicit caller value wins (a
      // dispatch-time snapshot is closer to the truth than a record
      // that may have been re-asserted since); `prev?.work` covers the
      // later minimal confirm from `removeWorktree`, which arrives
      // after the reap and would otherwise blank what the dispatch
      // recorded.
      const work = e.work ?? state.slugs[e.slug]?.work ?? prev?.work;
      // Same first-hand read as `work`, and for the same reason: the
      // reap takes the per-slug entry with the worktree, and a
      // post-merge `external` run outlives both — so this copy is what
      // a pause set before the merge still has to act through. A later
      // minimal confirm must not blank a pause toggled on the archived
      // row since, hence `prev` last rather than first.
      const automationsPaused = e.automationsPaused ??
        state.slugs[e.slug]?.automationsPaused ?? prev?.automationsPaused;
      bySlug.set(e.slug, {
        ...prev,
        slug: e.slug,
        branch: e.branch,
        removedAt: e.removedAt,
        ...(e.title !== undefined ? { title: e.title } : {}),
        ...(e.issueId !== undefined ? { issueId: e.issueId } : {}),
        ...(e.githubIssue !== undefined ? { githubIssue: e.githubIssue } : {}),
        ...(e.gitState !== undefined ? { gitState: e.gitState } : {}),
        ...(e.prNumber !== undefined ? { prNumber: e.prNumber } : {}),
        ...(e.prUrl !== undefined ? { prUrl: e.prUrl } : {}),
        ...(e.prState !== undefined ? { prState: e.prState } : {}),
        ...(work !== undefined ? { work } : {}),
        ...(automationsPaused === true ? { automationsPaused: true } : {}),
      });
    }
    const cutoff = Date.now() - REMOVED_MAX_AGE_MS;
    const removed = [...bySlug.values()]
      .filter((e) => (Date.parse(e.removedAt) || 0) >= cutoff)
      .sort((a, b) => b.removedAt.localeCompare(a.removedAt))
      .slice(0, REMOVED_MAX_ENTRIES);
    writeWtState({ ...state, removed });
  });
  // A merged worktree leaving the active list is the "this task fully
  // landed" moment — attention-worthy (and it toasts by default per the
  // logger contract). Only the rich dispatch-time snapshot carries
  // `prState`, so the later minimal confirm from `removeWorktree` never
  // re-emits; non-merged removals stay on the event feed, where the
  // destroy flows already narrate them.
  for (const e of entries) {
    if (!isMergedRemoval(e)) continue;
    const pr = e.prNumber !== undefined ? ` (#${e.prNumber})` : "";
    // Source carries the slug (feed convention) — don't repeat it in
    // the text.
    createLogger(e.slug).attention.ok(`merged${pr} — worktree archived`);
  }
}

/**
 * Drop a slug from the removed history. Called by `createWorktree` so a
 * restored / re-created slug stops appearing as removed. No-op when absent.
 */
export function clearRemovedWorktree(slug: string): void {
  withWtStateLock(() => {
    const state = readWtState();
    if (!state.removed.some((e) => e.slug === slug)) return;
    writeWtState({
      ...state,
      removed: state.removed.filter((e) => e.slug !== slug),
    });
  });
}

/**
 * The recently-removed JSON shape shared by `wt ls --json` and
 * `wt status --all --json`. `kind` (not `state`) discriminates these
 * from live rows: status --all's live rows already use `state` for the
 * WORK-state vocabulary, and overloading one key with two value
 * domains silently broke consumers filtering on it.
 */
export function removedJsonEntry(e: RemovedWorktree): {
  slug: string;
  branch: string;
  kind: "merged" | "removed";
  pr: number | null;
  pr_url: string | null;
  title: string | null;
  archived_at: string;
  work_state: string | null;
  verify_after_merge: string | null;
  verification_owed: boolean;
} {
  return {
    slug: e.slug,
    branch: e.branch,
    kind: isMergedRemoval(e) ? "merged" : "removed",
    pr: e.prNumber ?? null,
    pr_url: e.prUrl ?? null,
    title: e.title ?? null,
    archived_at: e.removedAt,
    // The status the row held when it went away. Flat here, matching
    // `wt status --all --json`'s convention rather than `wt fleet
    // --json`'s nested `work` object, because all three commands append
    // this same entry and it has to read identically on each.
    work_state: e.work?.state ?? null,
    verify_after_merge: e.work?.verifyAfterMerge ?? null,
    // The question anyone actually asks of a removed row, precomputed
    // so three consumers can't each derive it slightly differently.
    // False for an entry with no record at all, which is UNKNOWN rather
    // than fine — `work_state: null` is the tell, and it is why this
    // is not the only field.
    verification_owed: verificationOwedAtRemoval(e),
  };
}

/**
 * Did this row still owe a post-merge verification when its checkout
 * was taken? `verified` is the discharge and `dropped` voids it, same
 * as everywhere else; anything else with steps recorded means the check
 * never happened and the context is gone.
 *
 * Deliberately NOT `owesPostMergeVerification`: that one gates on the
 * branch having landed, which is a live-row question. By the time a row
 * is in this history the checkout is gone either way, and an obligation
 * on a branch that never landed is exactly as unresolved.
 */
export function verificationOwedAtRemoval(e: RemovedWorktree): boolean {
  const work = e.work;
  if (!work?.verifyAfterMerge) return false;
  return work.state !== "verified" && work.state !== "dropped";
}
