import { sameWorkClaim, type WorkStatusRecord } from "../work-status.ts";
import {
  isRemoteWorktreeLedgerKey,
  remoteWorktreeLedgerPrefix,
} from "../worktree-ref.ts";
import { readWtState, withWtStateLock, writeWtState } from "./io.ts";
import { GROUP_INBOX, stackIdFromSectionKey } from "./types.ts";
import type { WtSlugState, WtState } from "./types.ts";

/**
 * Drop dead groups from `sectionsOrder`: manual sections no slug
 * references. The inbox sentinel always survives (it's the migration
 * marker and the inbox is never deletable), and stack keys are kept
 * unconditionally — stacks are inferred from the live worktree list,
 * which this module doesn't see, so a stale key can't be told from a
 * momentarily-empty one. Stale stack keys are inert and tiny.
 */
function prunedSectionsOrder(state: WtState): string[] {
  const live = new Set<string>();
  for (const v of [...Object.values(state.slugs), ...Object.values(state.remoteLayouts)]) {
    if (v.section !== null) live.add(v.section);
  }
  return state.sectionsOrder.filter((s) => {
    if (s === GROUP_INBOX) return true;
    if (stackIdFromSectionKey(s) !== null) return true;
    return live.has(s);
  });
}

function ensureSection(state: WtState, section: string): WtState {
  if (state.sectionsOrder.includes(section)) return state;
  return { ...state, sectionsOrder: [...state.sectionsOrder, section] };
}

/** Drop a single slug's entry. No-op if absent. */
export function clearSlugState(slug: string): void {
  withWtStateLock(() => {
    const state = readWtState();
    if (!(slug in state.slugs)) return;
    const next = { ...state, slugs: { ...state.slugs } };
    delete next.slugs[slug];
    next.sectionsOrder = prunedSectionsOrder(next);
    writeWtState(next);
  });
}

/** Max order in a given section. Returns `null` when section is empty. */
function maxOrderIn(state: WtState, section: string | null): number | null {
  let max = -Infinity;
  for (const v of [...Object.values(state.slugs), ...Object.values(state.remoteLayouts)]) {
    if (v.section === section && v.order > max) max = v.order;
  }
  return Number.isFinite(max) ? max : null;
}

/** Min order in a given section. Returns `null` when section is empty. */
function minOrderIn(state: WtState, section: string | null): number | null {
  let min = Infinity;
  for (const v of [...Object.values(state.slugs), ...Object.values(state.remoteLayouts)]) {
    if (v.section === section && v.order < min) min = v.order;
  }
  return Number.isFinite(min) ? min : null;
}

/**
 * Place a slug at the top or bottom of a section. "Bottom" = max+1
 * (default for picker / generic assignment). "Top" = min-1, used by
 * Shift+J across a section boundary so the moved row lands adjacent
 * to where it was. Source section's other members keep their orders
 * and `sectionsOrder` is pruned of any section that just emptied.
 */
export function placeSlug(
  slug: string,
  section: string | null,
  position: "top" | "bottom",
): void {
  withWtStateLock(() => {
    let state = readWtState();
    if (section !== null) state = ensureSection(state, section);
    const next: WtState = { ...state, slugs: { ...state.slugs } };
    let order: number;
    if (position === "top") {
      const min = minOrderIn(next, section);
      order = min === null ? 0 : min - 1;
    } else {
      const max = maxOrderIn(next, section);
      order = max === null ? 0 : max + 1;
    }
    next.slugs[slug] = { ...next.slugs[slug], section, order };
    next.sectionsOrder = prunedSectionsOrder(next);
    writeWtState(next);
  });
}

/**
 * Record (or clear, with `base = null`) a worktree's fork base. Creates
 * the slug entry on first write so a brand-new worktree (no manual
 * section/order yet) can still carry its base.
 */
/**
 * Atomically claim the first still-unclaimed candidate port for `slug`.
 * The caller pre-filters `candidates` to OS-level-free ports (async
 * probes can't run under this sync flock); this re-reads the state
 * INSIDE the lock so two processes allocating concurrently can't both
 * persist the same port — the loser sees the winner's claim and takes
 * the next candidate. Returns the claimed port, or null when every
 * candidate got claimed in the meantime (caller rescans).
 */
export function claimDevPort(slug: string, candidates: readonly number[]): number | null {
  return withWtStateLock(() => {
    const state = readWtState();
    const claimed = new Set<number>();
    for (const [s, rec] of Object.entries(state.slugs)) {
      if (s !== slug && rec.devPort !== undefined) claimed.add(rec.devPort);
    }
    for (const port of candidates) {
      if (claimed.has(port)) continue;
      const next: WtState = { ...state, slugs: { ...state.slugs } };
      const entry: WtSlugState = { section: null, order: 0, ...state.slugs[slug] };
      entry.devPort = port;
      next.slugs[slug] = entry;
      writeWtState(next);
      return port;
    }
    return null;
  });
}

/**
 * Persist (or clear) the slug's allocated dev-server port. Mirrors
 * `setSlugBase`'s shape — creates the slug entry on first write, no-op
 * when clearing a slug with no record.
 */
export function setSlugDevPort(slug: string, port: number | null): void {
  withWtStateLock(() => {
    const state = readWtState();
    const prev = state.slugs[slug];
    if (!prev && port === null) return;
    const next: WtState = { ...state, slugs: { ...state.slugs } };
    const entry: WtSlugState = { section: null, order: 0, ...prev };
    if (port === null) delete entry.devPort;
    else entry.devPort = port;
    next.slugs[slug] = entry;
    writeWtState(next);
  });
}

/**
 * Record (or clear) the HEAD a dev server was started on. Written on
 * every start, so a restart re-anchors and the staleness signal is
 * about the CURRENT run rather than the first one ever.
 */
export function setSlugDevStartedSha(slug: string, sha: string | null): void {
  withWtStateLock(() => {
    const state = readWtState();
    const prev = state.slugs[slug];
    if (!prev && sha === null) return;
    const next: WtState = { ...state, slugs: { ...state.slugs } };
    const entry: WtSlugState = { section: null, order: 0, ...prev };
    if (sha === null) delete entry.devStartedSha;
    else entry.devStartedSha = sha;
    next.slugs[slug] = entry;
    writeWtState(next);
  });
}

/**
 * Record a fleet-level examination verdict against the sha it was
 * reached at. Overwrites any previous one — a verdict describes the
 * examiner's CURRENT conclusion, so keeping a history would just be a
 * log nobody reads. `null` clears it.
 */
export function setSlugExamined(
  slug: string,
  examined: { sha: string; baseSha?: string; verdict: string; by?: string; at: string } | null,
): void {
  withWtStateLock(() => {
    const state = readWtState();
    const prev = state.slugs[slug];
    if (!prev && examined === null) return;
    const next: WtState = { ...state, slugs: { ...state.slugs } };
    const entry: WtSlugState = { section: null, order: 0, ...prev };
    if (examined === null) delete entry.examined;
    else entry.examined = examined;
    next.slugs[slug] = entry;
    writeWtState(next);
  });
}

/** Called only after creation succeeds; restacks and ordinary status writes retain it. */
export function recordSlugCreated(slug: string, at = new Date().toISOString()): void {
  withWtStateLock(() => {
    const state = readWtState();
    const prev = state.slugs[slug];
    if (prev?.createdAt) return;
    writeWtState({ ...state, slugs: { ...state.slugs, [slug]: { section: null, order: 0, ...prev, createdAt: at } } });
  });
}

export function setSlugBase(
  slug: string,
  base: { branch: string; sha?: string } | null,
): void {
  withWtStateLock(() => {
    const state = readWtState();
    const prev = state.slugs[slug];
    if (!prev && !base) return;
    const next: WtState = { ...state, slugs: { ...state.slugs } };
    const entry: WtSlugState = { section: null, order: 0, ...prev };
    delete entry.baseBranch;
    delete entry.baseSha;
    if (base) {
      entry.baseBranch = base.branch;
      if (base.sha) entry.baseSha = base.sha;
    }
    next.slugs[slug] = entry;
    writeWtState(next);
  });
}

/**
 * Advance a slug's replay anchor, but only while its recorded parent
 * still matches what the replay resolved at chain time. A concurrent
 * writer — typically a background destroy reparenting this slug off a
 * just-deleted parent — wins: clobbering its rewrite with the stale
 * pre-destroy parent would leave the record pointing at a dead branch.
 * Returns false when the record moved and the anchor was left alone
 * (still valid — it names a tip that's an ancestor of the branch either
 * way; the next reconcile/replay recomputes from it).
 */
export function advanceBaseAnchor(
  slug: string,
  expectedParent: string,
  sha: string,
): boolean {
  return withWtStateLock(() => {
    const state = readWtState();
    const prev = state.slugs[slug];
    if (prev?.baseBranch !== expectedParent) return false;
    const next: WtState = { ...state, slugs: { ...state.slugs } };
    next.slugs[slug] = { ...prev, baseSha: sha };
    writeWtState(next);
    return true;
  });
}

/**
 * Reparent every slug's recorded fork base that points at `branch` onto
 * the deleted branch's OWN recorded base (falling back to `trunk`).
 * Called by destroy after the branch is deleted — a dangling record
 * would keep rendering the stack against a ref that no longer resolves.
 * Each dependent's `baseSha` anchor is PRESERVED: it still names the
 * tip the dependent's own commits sit on, which is what keeps the next
 * replay squash-safe after the parent landed. Returns the affected
 * slugs for logging.
 *
 * When `deletedSlug` is given, the deleted branch's own record (still
 * in the state file — per-slug entries are reaped later, at startup) is
 * consulted for the new parent, so a mid-chain removal re-links
 * grandchildren to their grandparent instead of flattening them to
 * trunk.
 */
export function reparentBaseReferences(
  branch: string,
  trunk: string,
  deletedSlug?: string,
): string[] {
  return withWtStateLock(() => {
    const state = readWtState();
    const affected = Object.entries(state.slugs)
      .filter(([slug, s]) => s.baseBranch === branch && slug !== deletedSlug)
      .map(([slug]) => slug);
    if (affected.length === 0) return affected;
    const deletedBase = deletedSlug ? state.slugs[deletedSlug]?.baseBranch : undefined;
    const newParent = deletedBase && deletedBase !== branch ? deletedBase : trunk;
    const next: WtState = { ...state, slugs: { ...state.slugs } };
    for (const slug of affected) {
      const entry: WtSlugState = { ...next.slugs[slug]! };
      entry.baseBranch = newParent;
      // baseSha intentionally kept — the squash-safe anchor.
      next.slugs[slug] = entry;
    }
    writeWtState(next);
    return affected;
  });
}

/**
 * Convenience for the common "assign this slug to that section, drop
 * it at the bottom" path used by the picker.
 */
/**
 * Attach/detach the secondary GitHub issue number (`wt new --gh`,
 * `wt issue <slug> --gh <n> | --clear-gh`). Null clears.
 */
export function setSlugGithubIssue(slug: string, issue: number | null): void {
  withWtStateLock(() => {
    const state = readWtState();
    const prev = state.slugs[slug];
    if (!prev && issue === null) return;
    const next: WtState = { ...state, slugs: { ...state.slugs } };
    const entry: WtSlugState = { section: null, order: 0, ...prev };
    delete entry.githubIssue;
    if (issue !== null) entry.githubIssue = issue;
    next.slugs[slug] = entry;
    writeWtState(next);
  });
}

/**
 * Set/clear the tracker-id override (`wt issue <slug> --id <ID>` /
 * `--clear-id`, TUI `I`). Null clears, restoring the slug-parsed
 * fallback. The id is uppercased here so the store holds one spelling.
 */
/**
 * Set, clear, or blank a slug's tracker-id override.
 *
 * `null` REMOVES the override (readers fall back to parsing the slug);
 * `""` stores an asserted none (readers report no issue at all). The
 * two are different answers on any slug that carries an id, which is
 * why `resolveIssueId` distinguishes absent from empty.
 */
export function setSlugIssueId(slug: string, id: string | null): void {
  withWtStateLock(() => {
    const state = readWtState();
    const prev = state.slugs[slug];
    if (!prev && id === null) return;
    const next: WtState = { ...state, slugs: { ...state.slugs } };
    const entry: WtSlugState = { section: null, order: 0, ...prev };
    delete entry.issueId;
    if (id !== null) entry.issueId = id.trim().toUpperCase();
    next.slugs[slug] = entry;
    writeWtState(next);
  });
}

export function setSlugSection(slug: string, section: string | null): void {
  placeSlug(slug, section, "bottom");
}

/**
 * Controller-side section assignment for either a local slug or a
 * host-qualified remote ledger key. Remote layout never enters `slugs`, whose
 * records remain owned by the machine executing those worktrees.
 */
export function setWorktreeSection(key: string, section: string | null): void {
  if (!isRemoteWorktreeLedgerKey(key)) {
    setSlugSection(key, section);
    return;
  }
  withWtStateLock(() => {
    let state = readWtState();
    if (section !== null) state = ensureSection(state, section);
    const max = maxOrderIn(state, section);
    const next: WtState = {
      ...state,
      remoteLayouts: { ...state.remoteLayouts },
    };
    next.remoteLayouts[key] = {
      section,
      order: max === null ? 0 : max + 1,
    };
    next.sectionsOrder = prunedSectionsOrder(next);
    writeWtState(next);
  });
}

/**
 * Set (or clear, with `record = null`) a slug's asserted work status
 * (`wt status` / the TUI `u` picker). Mirrors `setSlugGithubIssue`'s
 * create-on-first-write / no-op-when-absent shape. Validation (risk
 * required on ready, note rules) is the CALLER's job — this setter
 * only persists; keeping policy out of it lets the TUI picker be more
 * lenient than the agent-facing CLI.
 */
export function setSlugWorkStatus(
  slug: string,
  record: WorkStatusRecord | null,
): boolean {
  let wrote = false;
  withWtStateLock(() => {
    const state = readWtState();
    const prev = state.slugs[slug];
    if (!prev && record === null) return;
    // Idempotent re-assert: a record asserting the same claim as the
    // current one is a no-op that KEEPS the original `at`. Agents (and
    // their hooks) re-assert freely; without this every repeat bumps
    // the timestamp, and each bump re-narrates + re-toasts the same
    // news in every watching TUI. Which fields count is decided by
    // `sameWorkClaim`, whose classification is exhaustive over the
    // record type — a hand-written list here is what let two later
    // fields become unwritable.
    if (record !== null && prev?.work && sameWorkClaim(prev.work, record)) {
      return;
    }
    const next: WtState = { ...state, slugs: { ...state.slugs } };
    const entry: WtSlugState = { section: null, order: 0, ...prev };
    delete entry.work;
    if (record !== null) entry.work = record;
    next.slugs[slug] = entry;
    writeWtState(next);
    wrote = true;
  });
  // Whether anything was STORED, not whether the call succeeded. A call
  // that can legitimately do nothing has to say which happened, or
  // every caller downstream reports its own argument back as fact —
  // which is exactly how the drifted guard above stayed invisible.
  return wrote;
}

/**
 * Swap two slugs' order values within a single section bucket.
 * Renormalizes the bucket against `bucketDisplay` first so unstated
 * entries get materialized. The renormalization preserves the
 * section's current min order (anchors the new sequence at that
 * baseline) so section-display position is stable across the swap.
 */
export function swapOrders(
  slugA: string,
  slugB: string,
  section: string | null,
  bucketDisplay: readonly string[],
): void {
  withWtStateLock(() => {
    const state = readWtState();
    const next: WtState = { ...state, slugs: { ...state.slugs } };
    const min = minOrderIn(next, section);
    const baseline = min === null ? 0 : min;
    for (let i = 0; i < bucketDisplay.length; i++) {
      const slug = bucketDisplay[i]!;
      const prev = next.slugs[slug];
      next.slugs[slug] = { ...prev, section, order: baseline + i };
    }
    const a = next.slugs[slugA];
    const b = next.slugs[slugB];
    if (!a || !b) return;
    next.slugs[slugA] = { ...a, order: b.order };
    next.slugs[slugB] = { ...b, order: a.order };
    writeWtState(next);
  });
}

/**
 * Rename a section across every slug that references it, plus the
 * `sectionsOrder` index. No-op if `oldName === newName`, the trimmed
 * `newName` is empty, or no slug references `oldName`.
 *
 * Merge case (`newName` already exists as a different section): the
 * surviving slot is the *existing* `newName` — so renaming "X" to "Y"
 * lands all of X's slugs at the bottom of Y, in their existing
 * relative order, and the merged section keeps Y's display position.
 * Source orders get rewritten to `maxOrderIn(Y) + 1, +2, ...` so the
 * merge is collision-free and the display sequence is well-defined.
 */
export function renameSection(oldName: string, newName: string): void {
  const trimmed = newName.trim();
  if (!trimmed || trimmed === oldName) return;
  withWtStateLock(() => {
    const state = readWtState();
    const referenced = [...Object.values(state.slugs), ...Object.values(state.remoteLayouts)]
      .some((v) => v.section === oldName);
    if (!referenced && !state.sectionsOrder.includes(oldName)) return;
    const next: WtState = {
      ...state,
      slugs: { ...state.slugs },
      remoteLayouts: { ...state.remoteLayouts },
    };
    const layouts = (): Array<{
      key: string;
      value: { section: string | null; order: number };
      remote: boolean;
    }> => [
      ...Object.entries(next.slugs).map(([key, value]) => ({ key, value, remote: false })),
      ...Object.entries(next.remoteLayouts).map(([key, value]) => ({ key, value, remote: true })),
    ];
    const writeLayout = (
      key: string,
      value: { section: string | null; order: number },
      remote: boolean,
    ): void => {
      if (remote) next.remoteLayouts[key] = value;
      else next.slugs[key] = { ...next.slugs[key], ...value };
    };
    const isMerge =
      trimmed !== oldName &&
      (next.sectionsOrder.includes(trimmed) ||
        layouts().some(({ value }) => value.section === trimmed));
    if (isMerge) {
      // Source slugs in their current within-source display order
      // (ascending by `order`), so the merge appends them after Y's
      // existing items in a sensible sequence.
      const sourceSlugs = layouts()
        .filter(({ value }) => value.section === oldName)
        .sort((a, b) => a.value.order - b.value.order);
      const max = maxOrderIn(next, trimmed);
      let cursor = max === null ? 0 : max + 1;
      for (const { key, value, remote } of sourceSlugs) {
        writeLayout(key, { ...value, section: trimmed, order: cursor++ }, remote);
      }
      // Drop oldName from the index; trimmed already lives there.
      next.sectionsOrder = next.sectionsOrder.filter((s) => s !== oldName);
      // The merged-away key is gone; keep the target's fold state as-is.
      next.foldedSections = next.foldedSections.filter((s) => s !== oldName);
    } else {
      for (const { key, value, remote } of layouts()) {
        if (value.section === oldName) {
          writeLayout(key, { ...value, section: trimmed }, remote);
        }
      }
      // Replace oldName with trimmed in-place so the section keeps its
      // display position — and carries its folded state to the new name.
      next.sectionsOrder = next.sectionsOrder.map((s) => (s === oldName ? trimmed : s));
      next.foldedSections = next.foldedSections.map((s) => (s === oldName ? trimmed : s));
    }
    next.sectionsOrder = prunedSectionsOrder(next);
    writeWtState(next);
  });
}

/** Drop a manual section across local and remote controller layout. */
export function removeSection(name: string): number {
  return withWtStateLock(() => {
    const state = readWtState();
    const local = Object.entries(state.slugs).filter(([, value]) => value.section === name);
    const remote = Object.entries(state.remoteLayouts)
      .filter(([, value]) => value.section === name);
    if (
      local.length === 0 &&
      remote.length === 0 &&
      !state.sectionsOrder.includes(name)
    ) return 0;
    const next: WtState = {
      ...state,
      slugs: { ...state.slugs },
      remoteLayouts: { ...state.remoteLayouts },
      foldedSections: state.foldedSections.filter((section) => section !== name),
    };
    let cursor = (maxOrderIn(next, null) ?? -1) + 1;
    for (const [key, value] of local) {
      next.slugs[key] = { ...value, section: null, order: cursor++ };
    }
    for (const [key, value] of remote) {
      next.remoteLayouts[key] = { ...value, section: null, order: cursor++ };
    }
    next.sectionsOrder = prunedSectionsOrder(next);
    writeWtState(next);
    return local.length + remote.length;
  });
}

/**
 * Insert any inferred-stack key that's in `visualOrder` but not yet in
 * `order` at its real on-screen slot: right after its nearest preceding
 * visual group that IS already ranked (front if none). Inferred stacks
 * never pre-register in `sectionsOrder` (an unranked one sorts to the
 * top), so a reorder that names one as a landmark would otherwise have
 * no anchor. Seeding them at their visual position — rather than blindly
 * at the front — keeps a move relative to exactly what the user sees,
 * even with several never-moved stacks on screen at once. Only stack
 * keys are seeded; manual names + the inbox are always self-healed into
 * `sectionsOrder` at read time.
 */
function seedVisualStacks(
  order: readonly string[],
  visualOrder: readonly string[],
): string[] {
  const out = [...order];
  for (let i = 0; i < visualOrder.length; i++) {
    const g = visualOrder[i]!;
    if (out.includes(g)) continue;
    if (stackIdFromSectionKey(g) === null) continue;
    // No preceding visual anchor → front, EXCEPT past a top-ranked
    // Inbox: an empty Inbox is invisible (so never in `visualOrder`),
    // and seeding before it would persist the stack above the Inbox —
    // display-identical today, but it silently costs the Inbox its top
    // slot for when it next has rows.
    let anchor = out[0] === GROUP_INBOX ? 0 : -1;
    for (let j = i - 1; j >= 0; j--) {
      const at = out.indexOf(visualOrder[j]!);
      if (at >= 0) {
        anchor = at;
        break;
      }
    }
    out.splice(anchor + 1, 0, g);
  }
  return out;
}

/**
 * Reorder the group list: remove `key` and reinsert it immediately
 * before/after `pastKey`. Manual keys must already be present (they're
 * self-healed into `sectionsOrder` at read time); an inferred STACK key
 * is registered on demand — seeded at its visual slot via `visualOrder`
 * (the present groups in display order), so moving a never-moved stack
 * section works the same as any manual one. Returns false when a manual
 * key is absent, the keys are equal, or the result is a no-op. "Place
 * past" rather than "swap with array neighbor" so the caller can name
 * the next VISIBLE group as the landmark — an invisible group sitting
 * between (an empty inbox) gets jumped in one keypress instead of
 * producing a phantom no-change move. Member slugs keep their `order`
 * values; only the group's rank moves.
 */
export function moveGroupPast(
  key: string,
  pastKey: string,
  side: "before" | "after",
  visualOrder: readonly string[] = [],
): boolean {
  return withWtStateLock(() => {
    const state = readWtState();
    if (key === pastKey) return false;
    let order = seedVisualStacks(state.sectionsOrder, visualOrder);
    // Backstop: a stack key named as key/landmark but absent from
    // `visualOrder` (shouldn't happen — the caller derives both from the
    // same rows) still gets registered so the move can't silently fail.
    for (const k of [key, pastKey]) {
      if (!order.includes(k) && stackIdFromSectionKey(k) !== null) {
        // Register at the front, but never above a top-ranked Inbox
        // (same policy as `seedVisualStacks`).
        order =
          order[0] === GROUP_INBOX
            ? [order[0]!, k, ...order.slice(1)]
            : [k, ...order];
      }
    }
    if (!order.includes(key)) return false;
    const arr = order.filter((s) => s !== key);
    const at = arr.indexOf(pastKey);
    if (at < 0) return false;
    arr.splice(side === "before" ? at : at + 1, 0, key);
    if (arr.every((s, i) => s === state.sectionsOrder[i]) && arr.length === state.sectionsOrder.length) {
      return false;
    }
    writeWtState({ ...state, sectionsOrder: arr });
    return true;
  });
}

/**
 * Toggle whether a section is folded in the list, persisting it. Returns the
 * new folded state. Keyed by the section's key (a manual name or a stack's
 * synthetic `stackSectionKey`). A key for a since-deleted section is inert (no
 * row matches it, so it renders nothing) and is intentionally left in place
 * rather than reaped — harmless, and reaping risks dropping a fold while its
 * rows are momentarily absent during a refresh. `renameSection` does migrate
 * a manual key so a rename doesn't silently unfold.
 */
export function toggleSectionFolded(sectionKey: string): boolean {
  return withWtStateLock(() => {
    const state = readWtState();
    const folded = state.foldedSections.includes(sectionKey);
    const next: WtState = {
      ...state,
      foldedSections: folded
        ? state.foldedSections.filter((s) => s !== sectionKey)
        : [...state.foldedSections, sectionKey],
    };
    writeWtState(next);
    return !folded;
  });
}

/**
 * Idempotently set a section's folded state. Archive actions use this rather
 * than a toggle so two overlapping mutations can never reopen the Archived
 * block while trying to hide newly archived rows.
 */
export function setSectionFolded(sectionKey: string, folded: boolean): boolean {
  return withWtStateLock(() => {
    const state = readWtState();
    const alreadyFolded = state.foldedSections.includes(sectionKey);
    if (alreadyFolded === folded) return folded;
    const next: WtState = {
      ...state,
      foldedSections: folded
        ? [...state.foldedSections, sectionKey]
        : state.foldedSections.filter((s) => s !== sectionKey),
    };
    writeWtState(next);
    return folded;
  });
}

/**
 * Reap stale slug entries against the live slug set. Called after
 * destroys to keep the state file tidy. No-op when nothing to drop.
 */
export function reapWtState(liveSlugs: ReadonlySet<string>): void {
  withWtStateLock(() => {
    const state = readWtState();
    // Edges with a dead endpoint are satisfied (merged) or moot
    // (removed) — they disappear rather than dangling.
    const edges = state.edges.filter(
      (e) => liveSlugs.has(e.from) && liveSlugs.has(e.to),
    );
    let changed = edges.length !== state.edges.length;
    for (const k of Object.keys(state.slugs)) {
      if (!liveSlugs.has(k)) {
        changed = true;
        break;
      }
    }
    if (!changed) return;
    const next: WtState = { ...state, slugs: {}, edges };
    for (const [k, v] of Object.entries(state.slugs)) {
      if (liveSlugs.has(k)) next.slugs[k] = v;
    }
    next.sectionsOrder = prunedSectionsOrder(next);
    writeWtState(next);
  });
}

/**
 * Reap one reachable worker's controller-owned layout after a successful
 * inventory. SSH failure never calls this, so an offline host cannot be
 * mistaken for an empty one.
 */
export function reapRemoteLayouts(host: string, liveSlugs: ReadonlySet<string>): void {
  const prefix = remoteWorktreeLedgerPrefix(host);
  withWtStateLock(() => {
    const state = readWtState();
    const remoteLayouts = { ...state.remoteLayouts };
    let changed = false;
    for (const key of Object.keys(remoteLayouts)) {
      if (!key.startsWith(prefix)) continue;
      const encodedSlug = key.slice(prefix.length);
      let slug: string;
      try {
        slug = decodeURIComponent(encodedSlug);
      } catch {
        slug = encodedSlug;
      }
      if (liveSlugs.has(slug)) continue;
      delete remoteLayouts[key];
      changed = true;
    }
    if (!changed) return;
    const next = { ...state, remoteLayouts };
    next.sectionsOrder = prunedSectionsOrder(next);
    writeWtState(next);
  });
}

/**
 * Record a watched branch's tip, the watermark behind the
 * `branch.advanced` trigger.
 *
 * Called on two occasions and only two: the FIRST time a branch is
 * seen (recording it without firing, so an absent mark can never be
 * read as "everything up to here"), and after a fire has been
 * dispatched. Never on mere observation — the range between two tips is
 * consumed exactly once, and advancing the mark before the run is
 * delivered would drop it with nothing anywhere to say so.
 *
 * Returns whether it wrote, on the same reasoning as
 * `setSlugWorkStatus`: a call that can legitimately succeed by doing
 * nothing has to say which happened.
 */
export function setBranchTip(branch: string, sha: string): boolean {
  let wrote = false;
  withWtStateLock(() => {
    const state = readWtState();
    if (state.branchTips[branch] === sha) return;
    writeWtState({
      ...state,
      branchTips: { ...state.branchTips, [branch]: sha },
    });
    wrote = true;
  });
  return wrote;
}
