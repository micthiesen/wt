/**
 * Materialization + maintenance for stack manifests. wt owns the
 * manifest (truth); this module turns a planned manifest into real
 * worktrees, commits, and draft PRs (`applyStack`), reports the manifest
 * DAG against live reality (`stackStatus`), reconciles the manifest with
 * landed PRs (`reconcileStack`), and drives the native squash-safe engine
 * to replay slices onto their (possibly rewritten) parents (`replayStack`).
 * `rebaseStack` is the thin reconcile-then-replay convenience. The genuinely
 * hard part (anchored cherry-pick replay) lives in `RestackEngine`.
 */
import { mkdir } from "node:fs/promises";
import { dirname, join } from "node:path";

import { config } from "./config.ts";
import {
  branchExists,
  firstSha,
  gitQuiet,
  gitRun,
  localOrOriginRef,
  originBranchExists,
  revParse,
} from "./git.ts";
import {
  baseContent,
  DEFAULT_HUNK_CONTEXT,
  fileHunks,
  holisticBase,
  reconstructFile,
} from "./hunks.ts";
import { createWorktree } from "./lifecycle.ts";
import { tryAcquireLock, type LockHandle } from "./locks.ts";
import { createLogger } from "./logger.ts";
import {
  createDraftPr,
  retargetPrBase,
  viewPrInfo,
  type LivePrInfo,
} from "./github.ts";
import { backupBranchOwner, backupTimestamp, rebaseInProgress, restackEngine } from "./restack-engine.ts";
import {
  isTrunkBase,
  resolveParentBranch,
  topoSortSlices,
  transitiveAncestors,
} from "./stack-layout.ts";
import { dirSlug } from "./stage.ts";
import { verifyStack } from "./stack-verify.ts";
import {
  findStackIdByBranch,
  getStackManifest,
  patchStackManifest,
  putStackManifest,
  readWtState,
  setSlugBase,
  updateStackSlice,
  validateStackManifest,
  type PartialFile,
  type StackManifest,
  type StackSlice,
} from "./wtstate.ts";
import { listWorktrees, worktreeHasTrackedChanges } from "./worktree.ts";

const log = createLogger("[stack-ops]");

/** Flock slug serializing stack operations across processes. */
const STACK_LOCK_SLUG = "__stack__";

export type Logger = (line: string) => void;

/** Error every mutator returns/logs when the stack lock can't be had. */
const STACK_BUSY = "another wt stack operation is already running";

/**
 * Whether an existing PR matched by branch name should be ADOPTED on a
 * re-apply rather than triggering a fresh push + `gh pr create`. Only OPEN
 * (a prior run that pushed + opened but failed before recording) and MERGED
 * (already landed) qualify. A CLOSED PR must NOT be adopted: `gh` opens a
 * brand-new PR on the same branch, and a closed PR (e.g. a superseded re-split
 * slice whose branch name got reused) isn't tracking this work — adopting it
 * records a dead PR and skips materialization.
 *
 * `applyStack` MUST adopt MERGED here: a landed slice is re-recorded as merged
 * and skipped, never re-pushed. `addSliceToStack` is stricter — it REJECTS a
 * MERGED PR outright ("nothing left to stack"), because adding a brand-new tip
 * slice on an already-merged branch is nonsensical. The two paths agree only on
 * the negative case (CLOSED is never adoptable); they deliberately diverge on
 * MERGED, so this is not a shared predicate with `addSliceToStack`.
 */
export function isAdoptablePr(state: LivePrInfo["state"]): boolean {
  return state === "OPEN" || state === "MERGED";
}

/**
 * Acquire the cross-process stack lock, waiting briefly for a live holder
 * to finish. EVERY manifest mutator takes this — not just replay. Each
 * mutator does read-manifest → async git/gh work → write-manifest-back, so
 * two unserialized writers (a CLI `wt stack apply` racing the TUI's
 * reconcile) would silently lose whichever write lands first.
 */
async function acquireStackLock(phase: string): Promise<LockHandle | null> {
  const deadline = Date.now() + 5_000;
  for (;;) {
    const handle = tryAcquireLock(STACK_LOCK_SLUG, "stack", { phase });
    if (handle) return handle;
    if (Date.now() >= deadline) return null;
    await Bun.sleep(250);
  }
}

/**
 * Reproduce a slice's content as a single commit in its fresh worktree.
 * Whole-file slices (`slice.files`) are checked out from the holistic
 * branch on top of the parent; hunk-level slices (`slice.partials`) are
 * reconstructed from the holistic diff (see the partials block below).
 * Cumulatively the chain reproduces the holistic tree exactly.
 *
 * Edge cases the holistic checkout doesn't cover on its own:
 *  - files the holistic diff DELETES aren't on `holisticBranch`, so
 *    `checkout --` can't remove them — `git rm` handles those.
 *  - a file in the slice list that's neither on the holistic branch nor
 *    in the worktree is unreproducible and surfaces as an error.
 *
 * Rename contract: a rename `a -> b` on the holistic branch must list
 * BOTH paths in `slice.files` (old + new) so `a` is removed and `b`
 * checked out. The planner (`/split`) owns that; this only reproduces.
 */
async function materializeSliceCommit(
  wtPath: string,
  holisticBranch: string,
  slice: StackSlice,
  ancestorOwned: Map<string, Set<string>>,
  baseBySource: Map<string, string>,
  context: number,
  onLog: Logger,
): Promise<{ ok: true } | { ok: false; error: string }> {
  const present: string[] = [];
  const deleted: string[] = [];
  const missing: string[] = [];
  for (const f of slice.files) {
    if (await gitQuiet(["cat-file", "-e", `${holisticBranch}:${f}`], wtPath)) {
      present.push(f);
    } else if (await gitQuiet(["cat-file", "-e", `HEAD:${f}`], wtPath)) {
      // Absent on holistic but present in the parent tree → a deletion.
      deleted.push(f);
    } else {
      missing.push(f);
    }
  }
  if (missing.length > 0) {
    return {
      ok: false,
      error: `files not on ${holisticBranch} and not in worktree: ${missing.join(", ")}`,
    };
  }
  if (present.length > 0) {
    const r = await gitRun(["checkout", holisticBranch, "--", ...present], wtPath);
    if (r.exitCode !== 0) {
      return { ok: false, error: r.stderr.trim() || "checkout failed" };
    }
    onLog(`  checked out ${present.length} file(s) from ${holisticBranch}`);
  }
  for (const f of deleted) {
    const r = await gitRun(["rm", "-f", "--", f], wtPath);
    if (r.exitCode !== 0) {
      return { ok: false, error: `git rm ${f}: ${r.stderr.trim()}` };
    }
  }
  if (deleted.length > 0) onLog(`  removed ${deleted.length} deleted file(s)`);

  // Hunk-level files: reconstruct the exact intermediate content (base +
  // this slice's owned hunks + every ancestor's owned hunks) from the
  // holistic diff and write it. Cumulatively this reproduces the holistic
  // file at the slice that owns its last hunk. Pure text replay — no apply
  // fuzz, so this materialize step never conflicts (replaying a partial
  // slice onto a moved parent still can — that keeps its `/restack` bail).
  const partials = slice.partials ?? [];
  if (partials.length > 0) {
    // Reuse the base the coverage gate resolved for this source so the two
    // phases provably agree (linked worktrees share the object store, so a
    // SHA resolved in the main clone resolves here too).
    const base = baseBySource.get(holisticBranch) ?? (await holisticBase(wtPath, holisticBranch));
    for (const p of partials) {
      const fd = await fileHunks(wtPath, base, holisticBranch, p.file, context);
      if (fd.binary) {
        return { ok: false, error: `cannot hunk-split binary file ${p.file}` };
      }
      const known = new Set(fd.hunks.map((h) => h.id));
      const owned = new Set<string>([...(ancestorOwned.get(p.file) ?? []), ...p.hunks]);
      const unknown = p.hunks.filter((h) => !known.has(h));
      if (unknown.length > 0) {
        return {
          ok: false,
          error: `partial ${p.file}: hunk id(s) not in the holistic diff: ${unknown.join(", ")} (re-run \`wt stack hunks\`; the holistic branch may have changed)`,
        };
      }
      const raw = await baseContent(wtPath, base, p.file);
      // `git show`/`git diff` decode to UTF-8; a file with NUL bytes that git
      // didn't flag binary (rare, but possible) would corrupt on round-trip.
      // Refuse it rather than commit mangled content.
      if (raw.includes("\0")) {
        return { ok: false, error: `cannot hunk-split ${p.file}: base content is not valid UTF-8 text` };
      }
      const content = reconstructFile(raw, fd.hunks, owned);
      try {
        await mkdir(dirname(join(wtPath, p.file)), { recursive: true });
        await Bun.write(join(wtPath, p.file), content);
      } catch (e) {
        return { ok: false, error: `write ${p.file}: ${e instanceof Error ? e.message : String(e)}` };
      }
      const add = await gitRun(["add", "--", p.file], wtPath);
      if (add.exitCode !== 0) {
        return { ok: false, error: `git add ${p.file}: ${add.stderr.trim()}` };
      }
    }
    onLog(`  reconstructed ${partials.length} partial file(s) by hunk`);
  }
  // `checkout --` and `git rm` already stage every slice path, so no
  // extra `git add` is needed — and `add -A` would risk staging an
  // unrelated untracked file that happens to match a slice path.
  // An empty staging area means the slice adds nothing on top of its
  // parent (a mis-partitioned manifest); surface that clearly instead of
  // letting `git commit` fail with a generic status dump.
  if (await gitQuiet(["diff", "--cached", "--quiet"], wtPath)) {
    return { ok: false, error: `slice ${slice.id} produced no changes vs its parent` };
  }
  const commit = await gitRun(["commit", "-m", slice.title], wtPath);
  if (commit.exitCode !== 0) {
    return {
      ok: false,
      error: (commit.stderr || commit.stdout).trim() || "commit failed",
    };
  }
  return { ok: true };
}

/**
 * The hunks ALREADY present in this slice's base for each partial file: every
 * hunk owned by a transitive ancestor (which the chain commits below this
 * slice carry) PLUS every hunk owned by a MERGED slice (which trunk/base
 * already holds, even if that slice isn't a dependsOn-ancestor — e.g. an
 * earlier landed sibling that hunk-split the same file). Reconstruction writes
 * ABSOLUTE file content (base + this set + the slice's own hunks), so the
 * merged hunks must be in it or the commit would spuriously REVERT a landed
 * hunk when it diffs against the trunk parent that already contains it.
 */
export function ancestorOwnedHunks(
  manifest: StackManifest,
  slice: StackSlice,
  ancestors: Map<string, Set<string>>,
): Map<string, Set<string>> {
  const owned = new Map<string, Set<string>>();
  const anc = ancestors.get(slice.id) ?? new Set();
  for (const s of manifest.slices) {
    const include = anc.has(s.id) || s.status === "merged";
    if (!include) continue;
    for (const p of s.partials ?? []) {
      const set = owned.get(p.file) ?? new Set<string>();
      for (const h of p.hunks) set.add(h);
      owned.set(p.file, set);
    }
  }
  return owned;
}

/**
 * Before materializing, prove every partial file is reconstructible into a
 * correct stack tip. Three independent ways a manifest can be union-valid yet
 * still drop or mis-place holistic content (none catchable by the structural
 * `validateStackManifest`, which can't see the real diff):
 *  1. COVERAGE — each holistic hunk of the file owned by exactly one LIVE
 *     slice, OR already landed (owned by a MERGED slice → satisfied by base/
 *     trunk, which now contains it). No stale ids, not binary.
 *  2. CHAIN — the LIVE slices owning a file must lie on a single dependency
 *     chain, i.e. one owner ("tip") transitively descends from every other
 *     live owner. Materialize reconstructs "base + ancestor-owned + own"
 *     hunks, so only a slice that depends on all other owners ends up with the
 *     COMPLETE file. Two parallel-lane owners would each carry half and no tip
 *     the whole.
 *  3. SINGLE SOURCE — a file partitioned under two different `source`
 *     branches has two different holistic diffs; the owned-hunk maps merge by
 *     path and would mismatch. Forbid it.
 *
 * A MERGED slice's hunks are already in trunk/base, so it is NOT a live
 * reconstruction owner: it's excluded from the chain check (a live tip needn't
 * descend from a landed slice) and its hunks count as covered-by-base in the
 * coverage check (not "unassigned"). This is the partial-file analogue of how
 * `replayStack` already drops merged slices — the open slice reconstructs base
 * + its own hunks, and base (trunk) holds the merged slice's hunk, so the tip
 * is whole.
 *
 * Also resolves + caches each source's diff base SHA into `baseBySource` so
 * materialize reconstructs against the SAME base this gate validated.
 */
export async function validatePartialCoverage(
  manifest: StackManifest,
  cwd: string,
  ancestors: Map<string, Set<string>>,
  baseBySource: Map<string, string>,
): Promise<string | null> {
  const context = manifest.hunkContext ?? DEFAULT_HUNK_CONTEXT;
  const mergedIds = new Set(
    manifest.slices.filter((s) => s.status === "merged").map((s) => s.id),
  );
  // owners by file path. `liveOwners` drives the chain + coverage gates;
  // `mergedOwned` records hunks already in base so coverage doesn't flag them.
  const liveOwnersByFile = new Map<string, string[]>(); // file -> live slice ids
  const sourcesByFile = new Map<string, Set<string>>(); // file -> source branches (live only)
  const groups = new Map<string, { source: string; file: string; owned: Set<string>; mergedOwned: Set<string> }>();
  for (const s of manifest.slices) {
    const source = s.source ?? manifest.holisticBranch;
    const merged = mergedIds.has(s.id);
    for (const p of s.partials ?? []) {
      const key = `${source}\0${p.file}`;
      const g =
        groups.get(key) ??
        { source, file: p.file, owned: new Set<string>(), mergedOwned: new Set<string>() };
      groups.set(key, g);
      if (merged) {
        for (const h of p.hunks) g.mergedOwned.add(h);
        continue;
      }
      (liveOwnersByFile.get(p.file) ?? liveOwnersByFile.set(p.file, []).get(p.file)!).push(s.id);
      (sourcesByFile.get(p.file) ?? sourcesByFile.set(p.file, new Set()).get(p.file)!).add(source);
      for (const h of p.hunks) g.owned.add(h);
    }
  }
  // (3) single source per file (across live owners).
  for (const [file, sources] of sourcesByFile) {
    if (sources.size > 1) {
      return `partial ${file}: split across ${sources.size} source branches (${[...sources].join(", ")}) — a file's hunks must all come from one source`;
    }
  }
  // (2) LIVE owners of each file form a single dependency chain. Merged owners
  // dropped out above — a live tip needn't descend from a landed slice.
  for (const [file, owners] of liveOwnersByFile) {
    const set = new Set(owners);
    const tip = owners.find((o) => [...set].every((x) => x === o || ancestors.get(o)?.has(x)));
    if (!tip) {
      return `partial ${file}: owning slices (${owners.join(", ")}) don't form a dependency chain — no single slice descends from all of them, so no commit would carry the whole file`;
    }
  }
  // (1) coverage against the real per-source diff. A holistic hunk is covered
  // when a LIVE slice owns it OR a MERGED slice already landed it (it's in
  // base/trunk now). Only a hunk no slice claims at all is truly unassigned.
  for (const { source, file, owned, mergedOwned } of groups.values()) {
    let base = baseBySource.get(source);
    if (base === undefined) {
      base = await holisticBase(cwd, source);
      baseBySource.set(source, base);
    }
    const fd = await fileHunks(cwd, base, source, file, context);
    if (fd.binary) return `cannot hunk-split binary file ${file}`;
    const known = new Set(fd.hunks.map((h) => h.id));
    const missing = fd.hunks
      .filter((h) => !owned.has(h.id) && !mergedOwned.has(h.id))
      .map((h) => h.id);
    if (missing.length > 0) {
      return `partial ${file}: ${missing.length} holistic hunk(s) unassigned (${missing.join(", ")}) — every hunk must be owned by a slice`;
    }
    // Only LIVE-owned ids are checked for staleness. A MERGED slice's hunk is
    // content-hashed against the ORIGINAL fork point, but `base` is recomputed
    // live each apply and the source branch gets rebased onto post-merge trunk
    // (`/restack`). Once the fork point advances past the landed content, the
    // merged hunk is absorbed into base and DROPS OUT of `fileHunks(base,
    // source)` → it's no longer in `known`. That's the EXPECTED end state (the
    // hunk migrated into base), not drift, and the coverage/reconstruction
    // halves already tolerate it (base carries it). Flagging `mergedOwned` here
    // would re-fail every post-merge re-apply with a bogus "not in the diff".
    const stale = [...owned].filter((h) => !known.has(h));
    if (stale.length > 0) {
      return `partial ${file}: hunk id(s) not in the holistic diff: ${stale.join(", ")}`;
    }
  }
  return null;
}

/**
 * Before materializing, prove the manifest covers the ENTIRE holistic diff at
 * the file level: every changed path — a modify, an add, a DELETE, and BOTH
 * halves of a RENAME — is claimed by some slice's `files` or `partials`.
 *
 * `validateStackManifest` checks listed files are well-formed but can't see the
 * real diff, and the materializer reproduces only the paths a slice lists — so a
 * changed path no slice claims silently lingers from base on every slice and
 * breaks the one slice that removes whatever depended on it. The classic miss is
 * the delete-half of a rename: the planner lists the new path and forgets the
 * old one (the inventory collapses a rename to a single `{old => new}` line). The
 * hunk gate ({@link validatePartialCoverage}) enforces this for partial files;
 * this is its whole-file analogue, closing the asymmetry. We ERROR on an
 * unclaimed path (naming the slice that owns its rename counterpart) rather than
 * auto-attaching, so the partition stays explicit.
 *
 * Per source (a re-split sub-slice can carry its own `source`), the diff is
 * `git diff --name-status -M <base>..<source>`. A MERGED slice's files are in
 * trunk/base, so they count as covered, and a post-merge-absorbed path that fell
 * out of the live diff is tolerated — mirroring the hunk gate's merged handling.
 */
export async function validateFileCoverage(
  manifest: StackManifest,
  cwd: string,
  /** Test-only: pin a source's diff base, bypassing {@link coverageBase}. */
  baseOverride?: Map<string, string>,
): Promise<string | null> {
  const mergedIds = new Set(
    manifest.slices.filter((s) => s.status === "merged").map((s) => s.id),
  );
  // Per source: claimed paths split live (owner id for the hint) vs merged.
  const bySource = new Map<string, { live: Map<string, string>; merged: Set<string> }>();
  for (const s of manifest.slices) {
    const source = s.source ?? manifest.holisticBranch;
    const claims =
      bySource.get(source) ??
      bySource.set(source, { live: new Map(), merged: new Set() }).get(source)!;
    const paths = [...(s.files ?? []), ...(s.partials ?? []).map((p) => p.file)];
    for (const f of paths) {
      if (mergedIds.has(s.id)) claims.merged.add(f);
      else claims.live.set(f, s.id);
    }
  }

  for (const [source, claims] of bySource) {
    const base = baseOverride?.get(source) ?? (await coverageBase(manifest, source, cwd));
    // `core.quotePath=false` so non-ASCII paths come back literal and match the
    // manifest's raw paths (cf. `fileHunks`); `gitRun` (not `git`) so an
    // unresolvable ref returns a clean error instead of throwing a stack trace.
    const r = await gitRun(
      ["-c", "core.quotePath=false", "diff", "--name-status", "-M", `${base}..${source}`],
      cwd,
    );
    if (r.exitCode !== 0) {
      return `whole-file coverage: cannot diff ${source} against its base — ${r.stderr.trim() || "git diff failed"}`;
    }
    const changed = parseNameStatus(r.stdout);
    const unassigned = [...changed.keys()].filter(
      (p) => !claims.live.has(p) && !claims.merged.has(p),
    );
    if (unassigned.length === 0) continue;

    const detail = unassigned.map((p) => {
      const counterpart = changed.get(p);
      if (counterpart) {
        const owner = claims.live.get(counterpart);
        return owner
          ? `${p} (rename of ${counterpart}, owned by slice ${owner} — add ${p} to that slice)`
          : `${p} (rename counterpart of ${counterpart})`;
      }
      return p;
    });
    const where = source === manifest.holisticBranch ? "" : ` [source ${source}]`;
    return (
      `${unassigned.length} changed path(s) unassigned${where}: ${detail.join("; ")} — ` +
      "every changed file (including deletions and both halves of a rename) must be " +
      "claimed by a slice's `files` or `partials`"
    );
  }
  return null;
}

/**
 * The base a source's slices were partitioned against, i.e. where the whole
 * source-group forks from everything else: the merge-base of `source` with the
 * branch the group's lane root sits on. For a trunk-rooted stack this is the
 * trunk fork point (= {@link holisticBase}); for a stack on an unmerged parent
 * PR it's the parent branch; for a re-split source it's the original slice's
 * parent. Using the group's actual fork point (not trunk unconditionally) keeps
 * the coverage diff to exactly what these slices add, so ancestor commits' files
 * aren't surfaced as falsely "unassigned". Degrades to {@link holisticBase} if
 * the parent ref can't be resolved.
 */
async function coverageBase(
  manifest: StackManifest,
  source: string,
  cwd: string,
): Promise<string> {
  const group = manifest.slices.filter(
    (s) => (s.source ?? manifest.holisticBranch) === source,
  );
  const ids = new Set(group.map((s) => s.id));
  // The lane root is the group slice whose base points OUTSIDE the group (trunk,
  // an external parent branch, or — for a re-split — a slice in another group).
  const root = group.find((s) => !ids.has(s.base)) ?? group[0];
  if (!root) return holisticBase(cwd, source);
  const parentRef = isTrunkBase(root)
    ? `origin/${config.branch.base}`
    : await localOrOriginRef(resolveParentBranch(manifest, root));
  const mb = await gitRun(["merge-base", parentRef, source], cwd);
  const sha = mb.stdout.trim();
  return mb.exitCode === 0 && sha ? sha : holisticBase(cwd, source);
}

/**
 * Parse `git diff --name-status -M` output into `path -> rename counterpart`
 * (the other half of a rename) or `null`. A rename (`R<score>`) contributes BOTH
 * its old (deleted) and new (added) path, each pointing at the other; every other
 * status (`A`/`M`/`D`/`T`) contributes its single path with no counterpart. Pure
 * (no I/O) so the parsing is unit-testable; `-C` is not passed, so no `C` lines.
 */
export function parseNameStatus(out: string): Map<string, string | null> {
  const map = new Map<string, string | null>();
  for (const line of out.split("\n")) {
    if (!line.trim()) continue;
    const parts = line.split("\t");
    const status = parts[0] ?? "";
    if (status.startsWith("R") && parts.length >= 3) {
      map.set(parts[1]!, parts[2]!);
      map.set(parts[2]!, parts[1]!);
    } else if (parts.length >= 2) {
      map.set(parts[1]!, null);
    }
  }
  return map;
}

/** A minimal-but-valid PR body. Richer bodies are authored by a skill later. */
function sliceBody(manifest: StackManifest, slice: StackSlice): string {
  const lines = [
    `Part ${slice.ordinal} of the ${manifest.issue} stack.`,
    "",
    `Stacked on \`${resolveParentBranch(manifest, slice)}\`.`,
  ];
  if (slice.oversized && slice.oversizedReason) {
    lines.push("", `> Oversized (sanctioned): ${slice.oversizedReason}`);
  }
  return lines.join("\n");
}

export type ApplyOptions = {
  /** Run `pnpm install` per slice. Default false — slow; install where needed. */
  install?: boolean;
  /**
   * Before creating any branch/PR, reconstruct each cumulative slice prefix in
   * a throwaway worktree and run `config.stack.verifyCommand` against it; abort
   * on the first red prefix. Default false (CI is the normal gate). Restores
   * the compiles-at-every-slice guarantee that hunk-splitting forfeits.
   */
  verify?: boolean;
};

export type ApplyResult = {
  /** Slices newly materialized this run. */
  materialized: string[];
  /** Slices skipped because they were already open/merged. */
  skipped: string[];
  /** First fatal error, if any — apply stops at the first failure. */
  error: string | null;
};

/**
 * Materialize a planned manifest: for each slice in dependency order,
 * create its worktree off the resolved parent, reproduce its file set as
 * one commit, push, open a draft PR, record the PR into the manifest, and
 * track it in the engine. The wt list derives the parent relationship
 * from the manifest, so there's no separate display state to write. Tags
 * the holistic branch on success. Idempotent: slices already
 * `open`/`merged` are skipped.
 */
export async function applyStack(
  stackId: string,
  opts: ApplyOptions,
  onLog: Logger,
): Promise<ApplyResult> {
  const lock = await acquireStackLock("apply");
  if (!lock) return { materialized: [], skipped: [], error: STACK_BUSY };
  try {
    return await applyStackLocked(stackId, opts, onLog);
  } finally {
    lock.release();
  }
}

async function applyStackLocked(
  stackId: string,
  opts: ApplyOptions,
  onLog: Logger,
): Promise<ApplyResult> {
  const manifest = getStackManifest(stackId);
  if (!manifest) {
    return { materialized: [], skipped: [], error: `no stack manifest: ${stackId}` };
  }
  if (!manifest.holisticBranch) {
    return { materialized: [], skipped: [], error: `manifest ${stackId} has no holisticBranch` };
  }

  let ordered: StackSlice[];
  try {
    ordered = topoSortSlices(manifest);
  } catch (e) {
    return { materialized: [], skipped: [], error: e instanceof Error ? e.message : String(e) };
  }

  // Gate on partial-file coverage/chain before touching any git state: a
  // missing hunk or an off-chain owner would silently drop holistic content
  // from the tip. Only runs when the stack uses hunk-level slices. The
  // resolved diff bases are cached so materialize reconstructs against the
  // SAME base this gate validated.
  const ancestors = transitiveAncestors(manifest.slices);
  const baseBySource = new Map<string, string>();
  const hasPartials = manifest.slices.some((s) => (s.partials?.length ?? 0) > 0);
  if (hasPartials) {
    const coverageError = await validatePartialCoverage(
      manifest,
      config.paths.mainClone,
      ancestors,
      baseBySource,
    );
    if (coverageError) return { materialized: [], skipped: [], error: coverageError };
  }

  // Whole-file coverage gate: every changed path (incl. deletions + both halves
  // of a rename) must be claimed by some slice, else it lingers from base and
  // breaks the slice that removes whatever depended on it. Runs for every stack
  // (not just hunk-split ones); shares the resolved bases with the gate above.
  const fileCoverageError = await validateFileCoverage(manifest, config.paths.mainClone);
  if (fileCoverageError) {
    return { materialized: [], skipped: [], error: fileCoverageError };
  }

  // Opt-in compile gate: reconstruct each cumulative prefix in a throwaway
  // worktree and run the configured verify command, BEFORE any branch/PR is
  // created. Hunk-splitting can produce a prefix that doesn't compile (a slice
  // takes a body but not its import); this catches it without waiting for CI.
  if (opts.verify) {
    const verdict = await verifyStack(stackId, onLog);
    if (!verdict.ok) {
      return { materialized: [], skipped: [], error: verdict.error };
    }
    onLog(`verify: all ${verdict.prefixes.length} prefix(es) passed`);
  }

  const materialized: string[] = [];
  const skipped: string[] = [];

  for (const slice of ordered) {
    if (slice.status !== "planned") {
      skipped.push(slice.id);
      onLog(`skip ${slice.id} (${slice.status})`);
      continue;
    }
    const parentBranch = resolveParentBranch(manifest, slice);
    // A trunk-based slice branches off `origin/<trunk>` and targets its PR
    // at trunk; any other slice (a stacked child, or a root stacked on an
    // unmerged parent PR) branches off + targets its resolved parent
    // branch. The engine only tracks non-trunk parents.
    const onTrunk = isTrunkBase(slice);
    let parentRef: string;
    if (onTrunk) {
      parentRef = `origin/${config.branch.base}`;
    } else {
      // Prefer the local parent branch (a sibling slice just materialized
      // it, or it's the user's parent-PR worktree); fall back to the
      // remote-tracking ref when the parent exists only on origin, so
      // `git worktree add` resolves a real ref either way.
      parentRef = await localOrOriginRef(parentBranch);
    }

    // Idempotent re-run: if an OPEN (or already-merged) PR exists on this
    // branch (a prior run materialized + pushed + opened the PR but failed
    // before recording it), adopt it instead of creating a duplicate. A
    // CLOSED PR is NOT adopted — `gh` happily opens a fresh PR on the same
    // branch, and the closed one (e.g. a superseded re-split slice that reused
    // this branch name) wasn't tracking this work. Adopting it would record a
    // dead PR and skip the push+create, leaving the slice unmaterialized.
    // (`addSliceToStack` shares only this CLOSED-rejection; it goes further and
    // rejects MERGED too, whereas apply re-records a merged slice.) Fall through
    // to create otherwise — see the FIX-4 reset below for the stale-branch case.
    const existingPr = await viewPrInfo(slice.branch);
    if (existingPr && isAdoptablePr(existingPr.state)) {
      onLog(`adopt ${slice.id} → existing PR #${existingPr.number}`);
      updateStackSlice(stackId, slice.id, {
        pr: existingPr.number,
        status: existingPr.state === "MERGED" ? "merged" : "open",
      });
      materialized.push(slice.id);
      continue;
    }
    if (existingPr) {
      onLog(`existing PR #${existingPr.number} on ${slice.branch} is ${existingPr.state.toLowerCase()} — opening a fresh PR`);
    }

    onLog(`apply ${slice.id} → ${slice.branch} (off ${parentRef})`);

    // We're on the create/fall-through path: no adoptable PR, so this slice
    // must be materialized fresh from `parentRef`. If the branch already
    // exists (a superseded re-split slice whose CLOSED PR we just declined to
    // adopt, reusing the same name), `createWorktree` takes its branchExists
    // path — checking out the STALE tip and IGNORING `base`. Left uncorrected
    // that poisons everything below: `baseSha` would record the stale tip
    // (corrupting the squash-safe replay anchor) and `materializeSliceCommit`
    // would build the slice's files on top of stale content, so the fresh PR's
    // diff against `prBase` carries superseded content. The slice's content is
    // authoritative from `source`/the manifest, so discarding the stale branch
    // state is intended — reset the worktree HEAD to `parentRef` after create.
    const branchPreexisted = await branchExists(slice.branch);

    // Slices are install-free by design (a slice == a light worktree, no
    // node_modules), so do NOT add a per-slice typecheck/build gate here —
    // it can't run. Verification is the skill's job, done BEFORE apply in a
    // dep-having checkout; per-slice CI is the backstop. `--install` is an
    // explicit opt-in, default off.
    const created = await createWorktree(slice.branch, {
      base: parentRef,
      runInstall: opts.install === true,
      onLog: (l) => onLog(`  ${l}`),
    });
    if (!created.ok) {
      return { materialized, skipped, error: `create ${slice.branch}: ${created.reason}` };
    }

    if (branchPreexisted) {
      // Hard-reset the reused branch onto the fresh parent so HEAD == parent
      // tip, exactly as if `createWorktree` had honored `base`. Without this
      // the branchExists path leaves HEAD at the stale tip (see above).
      const reset = await gitRun(["reset", "--hard", parentRef], created.path);
      if (reset.exitCode !== 0) {
        return {
          materialized,
          skipped,
          error: `reset reused branch ${slice.branch} onto ${parentRef}: ${(reset.stderr || reset.stdout).trim()}`,
        };
      }
      onLog(`  reset reused branch ${slice.branch} → ${parentRef} (discarded stale tip)`);
    }

    // Record the squash-safe replay anchor: the parent tip this slice's
    // commit will sit on. `createWorktree` started the branch at `parentRef`
    // (and we reset a reused branch back to it above), so HEAD is that tip
    // right now, before the slice commit lands on top.
    const baseSha = await revParse("HEAD", created.path);

    // A re-split sub-slice reproduces its files from the original slice's
    // branch (`slice.source`) — which carries content the pre-split holistic
    // branch predates — rather than the manifest holistic.
    const mat = await materializeSliceCommit(
      created.path,
      slice.source ?? manifest.holisticBranch,
      slice,
      ancestorOwnedHunks(manifest, slice, ancestors),
      baseBySource,
      manifest.hunkContext ?? DEFAULT_HUNK_CONTEXT,
      onLog,
    );
    if (!mat.ok) {
      return { materialized, skipped, error: `materialize ${slice.id}: ${mat.error}` };
    }

    const push = await gitRun(["push", "-u", "origin", slice.branch], created.path);
    if (push.exitCode !== 0) {
      return {
        materialized,
        skipped,
        error: `push ${slice.branch}: ${(push.stderr || push.stdout).trim()}`,
      };
    }
    onLog(`  pushed ${slice.branch}`);

    // gh wants a branch name for --base: the trunk name for a trunk-based
    // slice, else the resolved parent branch (sibling slice or external
    // parent PR branch).
    const prBase = onTrunk ? config.branch.base : parentBranch;
    const pr = await createDraftPr({
      cwd: created.path,
      head: slice.branch,
      base: prBase,
      title: slice.title,
      body: sliceBody(manifest, slice),
    });
    if (!pr.ok) {
      return { materialized, skipped, error: `pr create ${slice.branch}: ${pr.error}` };
    }
    onLog(`  opened draft PR #${pr.number}`);
    updateStackSlice(stackId, slice.id, {
      pr: pr.number,
      status: "open",
      ...(baseSha ? { baseSha } : {}),
    });
    materialized.push(slice.id);
  }

  // Archive the holistic branch as a tag so the origin node survives the
  // user rm'ing its worktree. `-f` so re-apply re-points cleanly. On a
  // re-apply (e.g. `wt stack split` → `apply`) the holistic branch has usually
  // already been archived to its tag and deleted — the tag still anchors the
  // origin node, so skip silently instead of warning on an unresolvable ref.
  const tagName = `${stackId}-holistic`;
  const holisticResolves = await gitQuiet([
    "rev-parse",
    "--verify",
    "--quiet",
    `${manifest.holisticBranch}^{commit}`,
  ]);
  if (holisticResolves) {
    const tag = await gitRun(["tag", "-f", tagName, manifest.holisticBranch]);
    if (tag.exitCode === 0) {
      patchStackManifest(stackId, { archivedTag: `refs/tags/${tagName}` });
      onLog(`tagged holistic branch → refs/tags/${tagName}`);
    } else {
      onLog(`warn: could not tag holistic branch: ${tag.stderr.trim()}`);
    }
  } else if (!manifest.archivedTag) {
    onLog(`warn: holistic branch ${manifest.holisticBranch} not found and no archived tag to anchor the origin node`);
  }

  log.info("applied stack", { stackId, materialized, skipped });
  return { materialized, skipped, error: null };
}

// ---------- status ----------

export type SliceStatusRow = {
  slice: StackSlice;
  /** Branch the manifest intends this slice to stack on. */
  expectedBase: string;
  /** Live PR info from GitHub, or null when there's no PR / gh is absent. */
  live: LivePrInfo | null;
  /** Human description of any drift between manifest and reality; null when aligned. */
  drift: string | null;
};

export type StackStatusReport = {
  manifest: StackManifest;
  rows: SliceStatusRow[];
};

/**
 * Reconcile the manifest against live reality: for each slice, compare
 * the intended parent branch with the live PR base. Drift is reported,
 * never silently trusted in either direction.
 */
export async function stackStatus(stackId: string): Promise<StackStatusReport | null> {
  const manifest = getStackManifest(stackId);
  if (!manifest) return null;
  const rows = await Promise.all(
    manifest.slices
      .slice()
      .sort((a, b) => a.ordinal - b.ordinal)
      .map(async (slice): Promise<SliceStatusRow> => {
        const expectedBase = resolveParentBranch(manifest, slice);
        const live = slice.pr ? await viewPrInfo(slice.branch) : null;
        let drift: string | null = null;
        if (live && live.baseRefName && live.baseRefName !== expectedBase) {
          drift = `PR base is ${live.baseRefName}, manifest expects ${expectedBase}`;
        } else if (
          slice.status === "open" &&
          live &&
          live.state === "MERGED"
        ) {
          drift = `PR #${live.number} is merged but manifest says ${slice.status}`;
        } else if (slice.status === "open" && slice.pr && !live) {
          drift = `manifest records PR #${slice.pr} but GitHub has none`;
        }
        return { slice, expectedBase, live, drift };
      }),
  );
  return { manifest, rows };
}

// ---------- reconcile / replay / rebase ----------

export type RebaseOptions = {
  /** Trunk that landed roots reparent onto. Default `config.branch.base`. */
  onto?: string;
};

export type RebaseResult =
  | { ok: true; output: string }
  | {
      ok: false;
      conflict: boolean;
      error: string;
      failedBranch?: string;
      backupBranch?: string;
    };

/**
 * The one-shot /restack convenience: reconcile the manifest against landed
 * PRs, then replay every surviving slice onto its (possibly rewritten)
 * parent. `reconcileStack` and `replayStack` are exposed separately so the
 * skill can drive them step-by-step around a conflict (reconcile once,
 * replay → resolve → replay again).
 */
export async function rebaseStack(
  stackId: string,
  opts: RebaseOptions,
  onLog: Logger,
): Promise<RebaseResult> {
  const manifest = getStackManifest(stackId);
  if (!manifest) {
    return { ok: false, conflict: false, error: `no stack manifest: ${stackId}` };
  }
  const trunk = opts.onto ?? config.branch.base;
  await reconcileStack(stackId, trunk, onLog);
  return replayStack(stackId, { onto: trunk }, onLog);
}

/**
 * Replay every surviving slice onto its parent, squash-safe, in topological
 * order: rebase the slice's own commits in its own worktree, force-push, and
 * retarget its PR base to match the manifest. Bails clean on the first
 * conflict, naming the slice + the backup branch the engine left — wt never
 * auto-resolves. Pure git + gh; does NOT reconcile (run `reconcileStack`
 * first for a post-merge restack). Serialized across processes by a flock.
 */
export async function replayStack(
  stackId: string,
  opts: RebaseOptions,
  onLog: Logger,
): Promise<RebaseResult> {
  const handle = await acquireStackLock("replay");
  if (!handle) {
    return { ok: false, conflict: false, error: STACK_BUSY };
  }
  try {
    return await replayStackLocked(stackId, opts, onLog);
  } finally {
    handle.release();
  }
}

async function replayStackLocked(
  stackId: string,
  opts: RebaseOptions,
  onLog: Logger,
): Promise<RebaseResult> {
  const manifest = getStackManifest(stackId);
  if (!manifest) {
    return { ok: false, conflict: false, error: `no stack manifest: ${stackId}` };
  }
  const trunk = opts.onto ?? config.branch.base;

  // Topo order so each parent is replayed before its children; merged slices
  // drop out (their branch is gone, their children already reparented).
  let ordered: StackSlice[];
  try {
    ordered = topoSortSlices(manifest);
  } catch (e) {
    return { ok: false, conflict: false, error: e instanceof Error ? e.message : String(e) };
  }
  // Only OPEN slices replay. Merged ones dropped out at reconcile, and a
  // `planned` slice isn't materialized — no PR, and any branch/worktree
  // already sitting under it is hand-authored WIP the engine must neither
  // rebase nor gate on (a dirty planned tip used to block the whole stack).
  // Skip it loudly; it catches up at `wt stack apply` / `wt stack add`.
  const live = ordered.filter((s) => s.status === "open");
  for (const s of ordered) {
    if (s.status === "planned") {
      onLog(`skip ${s.id} (${s.branch}) — planned slice, not yet materialized`);
    }
  }
  const byId = new Map(manifest.slices.map((s) => [s.id, s]));

  // Each slice replays IN ITS OWN WORKTREE (HEAD rebases in place), so map
  // branch → path and refuse any dirty one up front — a rebase would clobber.
  // Only TRACKED changes block: untracked files ride through a rebase safely
  // (git refuses cleanly if one would be overwritten), and the workflow itself
  // drops untracked files like `prompt.txt` into slice worktrees.
  const pathByBranch = new Map(
    (await listWorktrees())
      .filter((w) => !w.isMain && w.branch)
      .map((w) => [w.branch, w.path] as const),
  );
  for (const s of live) {
    const p = pathByBranch.get(s.branch);
    if (!p) continue;
    if (await worktreeHasTrackedChanges(p)) {
      return {
        ok: false,
        conflict: false,
        error: `slice ${s.id} worktree ${p} (${s.branch}) has uncommitted changes to tracked files — commit or stash before restacking`,
      };
    }
    // A worktree left mid-rebase by an earlier interrupted run can read clean
    // via `git status --porcelain` (no unmerged paths), so the dirty check
    // alone misses it. Replaying into it would make the engine abort and
    // silently discard that in-flight state — refuse up front instead.
    if (await rebaseInProgress(p)) {
      return {
        ok: false,
        conflict: false,
        error: `slice worktree ${p} (${s.branch}) is mid-rebase from an unfinished run — finish or \`git rebase --abort\` it there before restacking`,
      };
    }
  }

  // Freshen origin so `origin/<trunk>` and any external-parent ref resolve to
  // their live tips before we rebase onto them. A failed fetch would silently
  // leave stale refs and replay every slice onto an outdated base, so bail.
  const fetched = await gitRun(["fetch", "origin", "--quiet"]);
  if (fetched.exitCode !== 0) {
    return {
      ok: false,
      conflict: false,
      error: `git fetch origin failed (${(fetched.stderr || fetched.stdout).trim() || `exit ${fetched.exitCode}`}) — refusing to replay onto possibly-stale refs`,
    };
  }

  // Pass 1: resolve each slice's worktree + anchor (the old parent tip its
  // commits sit on) BEFORE any rewrite — so the merge-base fallback sees
  // pre-replay tips, AND so a missing worktree or unresolvable anchor fails
  // the whole run before a single slice has been pushed.
  const anchorById = new Map<string, string>();
  for (const s of live) {
    const p = pathByBranch.get(s.branch);
    if (!p) {
      return {
        ok: false,
        conflict: false,
        error: `slice ${s.id} (${s.branch}) has no worktree — recreate it with \`wt stack apply ${stackId}\``,
      };
    }
    const anchor = await resolveAnchor(s, byId, trunk, p);
    if (!anchor) {
      return {
        ok: false,
        conflict: false,
        error: `could not resolve a replay anchor for ${s.branch} (no baseSha and no merge-base)${plannedParentHint(s, byId)}`,
      };
    }
    anchorById.set(s.id, anchor);
  }

  // Pass 2: replay top-down, threading each slice's new tip to its children.
  // Worktree + anchor are guaranteed present from pass 1.
  const newTipById = new Map<string, string>();
  let replayed = 0;
  for (const s of live) {
    const worktreePath = pathByBranch.get(s.branch)!;
    const anchor = anchorById.get(s.id)!;
    const newBase = await resolveNewBaseSha(s, byId, trunk, newTipById, worktreePath);
    if (!newBase) {
      return {
        ok: false,
        conflict: false,
        error: `could not resolve the new base for ${s.branch}${plannedParentHint(s, byId)}`,
      };
    }

    onLog(`replay ${s.id} (${s.branch})`);
    const out = await restackEngine.replaySlice(
      { branch: s.branch, worktreePath, anchor, newBase },
      onLog,
    );
    if (!out.ok) {
      // Persist the failure to the daily app log — the engine only streams to
      // the console `onLog`, so a replay run from the CLI would otherwise leave
      // nothing to diagnose after the fact.
      log.warn("replay slice failed", {
        stackId,
        slice: s.id,
        branch: s.branch,
        conflict: out.conflict,
        worktree: worktreePath,
        anchor,
        newBase,
        error: out.error,
        // backupBranch only exists on the conflict variant — it's the recovery
        // handle, so log it when present.
        ...(out.conflict ? { backupBranch: out.backupBranch } : {}),
      });
      if (out.conflict) {
        return {
          ok: false,
          conflict: true,
          // `out.error` carries the engine's conflicting-file detail; keep it
          // so the operator sees WHICH files clashed at the CLI, not just in
          // the log.
          error: `${out.error} — resolve in its worktree, then re-run`,
          failedBranch: s.branch,
          backupBranch: out.backupBranch,
        };
      }
      return { ok: false, conflict: false, error: out.error };
    }
    newTipById.set(s.id, out.newTip);
    // Advance the stored anchor to the parent we just landed on, so the next
    // restack is a cheap no-op when nothing has moved.
    updateStackSlice(stackId, s.id, { baseSha: out.newBaseSha });

    // Keep the PR base aligned with the manifest when the slice actually
    // moved (a parent that landed/rewrote shifts the child onto a new base)
    // OR when the engine synced a stale remote — a hand-resolved conflict may
    // have changed the parent too. A slice that neither moved nor pushed has
    // a correct base already, so we skip the `gh pr view` probe — any
    // residual drift still surfaces in `wt stack status`.
    if (out.moved) replayed++;
    if (out.moved || out.pushed) {
      await retargetIfNeeded(s, resolveParentBranch(manifest, s), onLog);
    }
  }

  return { ok: true, output: `replayed ${replayed}/${live.length} slice(s)` };
}

/**
 * The parent-tip SHA a slice currently sits on (the `rebase --onto` anchor):
 * the *descendant-most* of the stored `baseSha` and the live merge-base of the
 * slice with its current parent ref — computed before any replay so siblings
 * haven't moved.
 *
 * The stored `baseSha` is the squash-safe cut point (the parent tip this slice's
 * commits were last based on). A conflict bail hands resolution to a human, who
 * rebases the slice by hand and force-pushes WITHOUT updating the manifest, so
 * the stored anchor goes stale. Replaying off a stale anchor re-applies the
 * parent's already-present commits onto themselves, a bogus conflict on an
 * already-correct slice.
 *
 * Two ways the stored anchor goes stale, and why a bare `--is-ancestor` guard is
 * not enough (it bit eng-5182 twice, then eng-5244 again):
 *
 *  1. Hand-rebased OFF the anchor entirely → `baseSha` is no longer an ancestor
 *     of the branch. Caught by the ancestor check; fall back to the merge-base.
 *  2. Hand-rebased onto NEWER trunk that itself descends from `baseSha` (main
 *     advanced mid-restack, or a fix-then-rebase-onto-fresh-main). `baseSha` is
 *     STILL an ancestor of the branch, so the ancestor check passes — but the
 *     real fork point has moved up to the live merge-base, which sits ABOVE
 *     `baseSha`. Cutting at the old anchor replays all of trunk's squashed
 *     history. The naive guard misses this.
 *
 * So when BOTH `baseSha` and the live merge-base are ancestors of the branch,
 * pick whichever is the descendant: the live merge-base wins after a rebase onto
 * newer trunk (case 2), `baseSha` wins in the healthy squash-merge case (its
 * commits sit ABOVE the pre-squash merge-base, so it stays the cut point that
 * excludes the squash-merged parent). Self-healing, no manual bookkeeping.
 */
export async function resolveAnchor(
  slice: StackSlice,
  byId: Map<string, StackSlice>,
  trunk: string,
  cwd: string,
): Promise<string | null> {
  const parentRef = currentParentRef(slice, byId, trunk);
  const mb = await gitRun(["merge-base", slice.branch, parentRef], cwd);
  const liveAnchor = mb.exitCode === 0 && mb.stdout.trim() ? mb.stdout.trim() : null;

  if (slice.baseSha) {
    const storedIsAncestor = await gitQuiet(
      ["merge-base", "--is-ancestor", slice.baseSha, slice.branch],
      cwd,
    );
    if (storedIsAncestor) {
      if (!liveAnchor) return slice.baseSha;
      // Both anchor the branch; use the more recent cut point. `baseSha` below
      // the live merge-base means the branch was rebased onto newer trunk —
      // the live merge-base is the true fork point (case 2). Otherwise the live
      // merge-base is at/below `baseSha` (squash case), so `baseSha` stands.
      const baseShaBelowLive = await gitQuiet(
        ["merge-base", "--is-ancestor", slice.baseSha, liveAnchor],
        cwd,
      );
      return baseShaBelowLive ? liveAnchor : slice.baseSha;
    }
  }
  return liveAnchor;
}

/**
 * Failure hint for a slice whose parent is still `planned`: replay skips
 * planned slices, so the parent's branch may not exist yet — the likely
 * reason an anchor or new base failed to resolve.
 */
function plannedParentHint(slice: StackSlice, byId: Map<string, StackSlice>): string {
  const parent = byId.get(slice.base);
  return parent?.status === "planned"
    ? ` — parent ${parent.id} is still planned; materialize it with \`wt stack apply\` first`
    : "";
}

/** The ref naming a slice's parent tip as it stands now (pre-replay). */
function currentParentRef(
  slice: StackSlice,
  byId: Map<string, StackSlice>,
  trunk: string,
): string {
  if (isTrunkBase(slice)) return `origin/${trunk}`;
  const sibling = byId.get(slice.base);
  if (sibling) return sibling.branch;
  return slice.base; // external parent branch
}

/**
 * The SHA to rebase a slice ONTO this run: trunk's freshly-fetched tip, the
 * parent sibling's just-replayed tip, or an external parent branch's live
 * tip (a stack stacked on an unmerged parent PR / another stack's tip).
 */
async function resolveNewBaseSha(
  slice: StackSlice,
  byId: Map<string, StackSlice>,
  trunk: string,
  newTipById: Map<string, string>,
  cwd: string,
): Promise<string | null> {
  if (isTrunkBase(slice)) return revParse(`origin/${trunk}`, cwd);
  const sibling = byId.get(slice.base);
  if (sibling) {
    const replayed = newTipById.get(sibling.id);
    return replayed ?? revParse(sibling.branch, cwd);
  }
  // External parent branch: prefer the local checkout, fall back to origin.
  return firstSha(cwd, [slice.base, `origin/${slice.base}`]);
}

/** Retarget a slice's PR base to `expectedBase` when GitHub disagrees. */
async function retargetIfNeeded(
  slice: StackSlice,
  expectedBase: string,
  onLog: Logger,
): Promise<void> {
  if (!slice.pr) return;
  const live = await viewPrInfo(slice.branch);
  if (!live || live.baseRefName === expectedBase) return;
  const r = await retargetPrBase(slice.pr, expectedBase);
  if (r.ok) onLog(`  retargeted PR #${slice.pr} base → ${expectedBase}`);
  else onLog(`  warn: retarget PR #${slice.pr} base: ${r.error}`);
}

/**
 * Reconcile the manifest against landed reality: flip merged slices to
 * `merged`, reparent each orphaned child onto its deepest surviving
 * dependency (or trunk), and reparent a slice whose EXTERNAL parent
 * (stack-on-stack) has landed onto trunk. Manifest bookkeeping only — reads
 * GitHub/git state but never rewrites branches — so the skill can run it on
 * its own before deciding to replay.
 */
export async function reconcileStack(
  stackId: string,
  trunk: string,
  onLog: Logger,
): Promise<void> {
  const lock = await acquireStackLock("reconcile");
  if (!lock) {
    onLog(`skipped reconcile of ${stackId} — ${STACK_BUSY}`);
    return;
  }
  try {
    await reconcileStackLocked(stackId, trunk, onLog);
  } finally {
    lock.release();
  }
}

async function reconcileStackLocked(
  stackId: string,
  trunk: string,
  onLog: Logger,
): Promise<void> {
  const manifest = getStackManifest(stackId);
  if (!manifest) return;
  // Probe live PR state for every candidate slice in parallel.
  const candidates = manifest.slices.filter(
    (s) => s.pr && s.status !== "merged",
  );
  const probed = await Promise.all(
    candidates.map(async (s) => ({ s, live: await viewPrInfo(s.branch) })),
  );
  const mergedIds = new Set<string>(
    manifest.slices.filter((s) => s.status === "merged").map((s) => s.id),
  );
  for (const { s, live } of probed) {
    if (live?.state === "MERGED") {
      mergedIds.add(s.id);
      updateStackSlice(stackId, s.id, { status: "merged" });
      onLog(`slice ${s.id} merged (#${s.pr})`);
    }
  }
  if (mergedIds.size > 0) {
    // Reparent each surviving slice that lost a dependency onto its
    // deepest STILL-OPEN dependency (highest ordinal), falling to trunk
    // only when none survive. Reparenting straight to trunk would flatten
    // a slice that still has a live ancestor (diamond / multi-parent).
    const fresh = getStackManifest(stackId);
    if (!fresh) return;
    const byId = new Map(fresh.slices.map((s) => [s.id, s]));
    for (const slice of fresh.slices) {
      if (slice.status === "merged") continue;
      const dependsOn = slice.dependsOn.filter((d) => !mergedIds.has(d));
      const baseMerged = mergedIds.has(slice.base);
      if (dependsOn.length === slice.dependsOn.length && !baseMerged) continue;
      const survivingParent = dependsOn
        .map((d) => byId.get(d))
        .filter((s): s is StackSlice => !!s)
        .sort((a, b) => b.ordinal - a.ordinal)[0];
      const base = survivingParent ? survivingParent.id : trunk;
      // The list reads the parent straight from the manifest, so updating
      // `base`/`dependsOn` is all that's needed — no separate display state.
      updateStackSlice(stackId, slice.id, { dependsOn, base });
      onLog(`reparented ${slice.id} onto ${base}`);
    }
  }

  // Cross-stack reconcile: a slice stacked on an EXTERNAL parent (another
  // stack's tip, or a standalone parent PR branch) keeps a dead `base` once
  // that parent lands — the own-slice probe above only sees THIS manifest's
  // PRs, so it can't notice. Detect the external parent merged (or its
  // branch gone) and reparent onto trunk. The slice's `baseSha` anchor keeps
  // the subsequent replay squash-safe: the landed parent's commits sit below
  // the anchor and are excluded by construction, exactly like a sibling
  // squash-merge. Runs unconditionally — the external parent merging is
  // invisible to `mergedIds`.
  const after = getStackManifest(stackId);
  if (!after) return;
  const siblingIds = new Set(after.slices.map((s) => s.id));
  const siblingBranches = new Set(after.slices.map((s) => s.branch));
  for (const slice of after.slices) {
    if (slice.status === "merged") continue;
    if (slice.base === trunk || isTrunkBase(slice)) continue;
    if (siblingIds.has(slice.base) || siblingBranches.has(slice.base)) continue;
    const live = await viewPrInfo(slice.base);
    if (live?.state === "MERGED") {
      updateStackSlice(stackId, slice.id, { base: trunk });
      onLog(`external parent ${slice.base} merged (#${live.number}) — reparented ${slice.id} onto ${trunk}`);
    } else if (!live && !(await branchExists(slice.base))) {
      // No PR and no branch anywhere — the parent is gone. (A CLOSED PR or a
      // still-open parent leaves the link alone.) The `branchExists`
      // corroboration is LOAD-BEARING, not belt-and-braces: `viewPrInfo`
      // returns null for a transient gh failure exactly as it does for
      // "no PR", and without the second check a gh hiccup would reparent
      // a slice whose parent is alive. (The MERGED branch above needs no
      // such guard — a failed probe can never read as MERGED.)
      updateStackSlice(stackId, slice.id, { base: trunk });
      onLog(`external parent ${slice.base} is gone — reparented ${slice.id} onto ${trunk}`);
    }
  }
}

// ---------- split (reshape a live stack) ----------

/** One sub-slice in a `splitStack` fragment — the planner (`/split`) authors these. */
export type SubSliceSpec = {
  id: string;
  title: string;
  branch: string;
  files: string[];
  partials?: PartialFile[];
  oversized?: boolean;
  oversizedReason?: string;
};

export type SplitResult =
  | {
      ok: true;
      newSliceIds: string[];
      /** Branch the new sub-slices reproduce their files from at materialize. */
      sourceBranch: string;
      /** Branch + PR of the replaced slice — supersede (close PR, delete branch) after apply. */
      supersededBranch: string;
      supersededPr: number | null;
      /** Branches of children re-threaded onto the new tip (need a `replay`). */
      rethreadedChildren: string[];
      /** The reshaped slice list (returned for `--plan` preview; written unless planning). */
      slices: StackSlice[];
    }
  | { ok: false; error: string };

/**
 * Reshape a live stack: replace one OPEN (or still-planned) slice with the N
 * sub-slices in `fragment`, chaining them in order and re-threading the
 * replaced slice's children onto the LAST sub-slice (the new tip). Pure
 * manifest bookkeeping — no git, no PRs — mirroring `reconcileStack`. The
 * caller then runs `wt stack apply` to materialize the new sub-slice branches
 * (sourced from the replaced slice's own branch, recorded as their `source`,
 * since it carries a refactor the pre-split holistic branch predates) and
 * `wt stack replay`/`R` to rebase the descendants onto the new tip.
 *
 * The replaced slice is REMOVED from the manifest, but its branch/PR are left
 * on GitHub: the branch is the materialize source, so it must survive until
 * `apply` — close the PR + delete the branch as superseded afterwards.
 *
 * Strictly validates the reshaped manifest via `validateStackManifest`
 * (catches duplicate ids/branches, dangling deps, empty file sets) before
 * writing. `opts.plan` validates + returns the new shape WITHOUT writing.
 */
export function splitStack(
  stackId: string,
  sliceId: string,
  fragment: SubSliceSpec[],
  opts: { plan?: boolean },
): SplitResult {
  // Sync function, so no patient wait — a non-blocking probe is enough to
  // keep a reshape from interleaving with a live replay/apply. A pure
  // `--plan` preview writes nothing and can skip the lock.
  const lock = opts.plan ? null : tryAcquireLock(STACK_LOCK_SLUG, "stack", { phase: "split" });
  if (!opts.plan && !lock) return { ok: false, error: STACK_BUSY };
  try {
    return splitStackLocked(stackId, sliceId, fragment, opts);
  } finally {
    lock?.release();
  }
}

function splitStackLocked(
  stackId: string,
  sliceId: string,
  fragment: SubSliceSpec[],
  opts: { plan?: boolean },
): SplitResult {
  const manifest = getStackManifest(stackId);
  if (!manifest) return { ok: false, error: `no stack manifest: ${stackId}` };
  const target = manifest.slices.find((s) => s.id === sliceId);
  if (!target) return { ok: false, error: `no slice "${sliceId}" in ${stackId}` };
  if (target.status === "merged") {
    return { ok: false, error: `slice ${sliceId} is merged — cannot re-split a landed slice` };
  }
  if (fragment.length < 2) {
    return { ok: false, error: `split needs ≥2 sub-slices (got ${fragment.length})` };
  }
  // Reusing the replaced slice's own id/branch would make a sub-slice source
  // itself — reject it so the partition stays unambiguous.
  for (const spec of fragment) {
    if (spec.id === target.id) return { ok: false, error: `sub-slice id "${spec.id}" reuses the slice being split` };
    if (spec.branch === target.branch) return { ok: false, error: `sub-slice branch "${spec.branch}" reuses the slice being split` };
  }

  // The sub-slices reproduce their files from the original slice's branch (it
  // carries the refactor); a still-planned target has no branch yet, so fall
  // back to whatever the target itself would have materialized from.
  const source = target.status === "planned" ? target.source : target.branch;

  // Build the sub-chain: the first sub-slice takes the target's place in the
  // graph (inherits its base + dependsOn); each later one stacks on the
  // previous. All start planned, content sourced from the original branch.
  const lastSubId = fragment[fragment.length - 1]!.id;
  const subSlices: StackSlice[] = fragment.map((spec, i) => ({
    id: spec.id,
    ordinal: 0, // renumbered below
    title: spec.title,
    branch: spec.branch,
    base: i === 0 ? target.base : fragment[i - 1]!.id,
    dependsOn: i === 0 ? [...target.dependsOn] : [fragment[i - 1]!.id],
    files: spec.files,
    ...(spec.partials && spec.partials.length > 0 ? { partials: spec.partials } : {}),
    pr: null,
    status: "planned" as const,
    oversized: spec.oversized === true,
    ...(spec.oversizedReason ? { oversizedReason: spec.oversizedReason } : {}),
    ...(source ? { source } : {}),
  }));

  // Re-thread the target's children onto the new tip (the last sub-slice).
  // A child may reference the target by slice id OR by branch name — `base`
  // is stored either way (`dependsOn` is always ids) — so match both and
  // normalize the rewrite to the sub-slice id (`resolveParentBranch` resolves
  // it to the branch, and the id is stable before the branch is materialized).
  const rethreaded: string[] = [];
  const refsTarget = (ref: string): boolean => ref === target.id || ref === target.branch;
  const rethread = (s: StackSlice): StackSlice => {
    const changed = refsTarget(s.base) || s.dependsOn.some(refsTarget);
    if (changed) rethreaded.push(s.branch);
    return {
      ...s,
      base: refsTarget(s.base) ? lastSubId : s.base,
      dependsOn: s.dependsOn.map((d) => (refsTarget(d) ? lastSubId : d)),
    };
  };

  // Splice the sub-chain into the target's slot (preserving display order),
  // re-threading every other slice, then renumber ordinals over the new order.
  // The branch `-NN-` token is historical and may now diverge from `ordinal`.
  const inOrder = [...manifest.slices].sort((a, b) => a.ordinal - b.ordinal);
  const reshaped: StackSlice[] = [];
  for (const s of inOrder) {
    if (s.id === target.id) reshaped.push(...subSlices);
    else reshaped.push(rethread(s));
  }
  reshaped.forEach((s, i) => {
    s.ordinal = i + 1;
  });

  const next: StackManifest = { ...manifest, slices: reshaped };
  const v = validateStackManifest(next);
  if (!v.ok) {
    return { ok: false, error: `reshaped manifest invalid:\n  ${v.errors.join("\n  ")}` };
  }
  if (!opts.plan) putStackManifest(v.manifest);
  return {
    ok: true,
    newSliceIds: subSlices.map((s) => s.id),
    sourceBranch: source ?? manifest.holisticBranch,
    supersededBranch: target.branch,
    supersededPr: target.pr,
    rethreadedChildren: rethreaded,
    slices: v.manifest.slices,
  };
}

// ---------- add (append an existing branch to a live stack) ----------

export type AddSliceResult =
  | {
      ok: true;
      slice: StackSlice;
      /** Branch the new slice stacks on (a sibling's branch, or trunk). */
      parentBranch: string;
      /** Whether the slice's PR pre-existed or `add` opened it. */
      prAction: "adopted" | "created";
    }
  | { ok: false; error: string };

/** Fallback slice title from a branch name: strip the namespace + issue-id
 *  (+ optional ordinal) prefix, de-kebab the rest. `--title` overrides. */
function titleFromBranch(branch: string): string {
  const tail = branch.split("/").pop() ?? branch;
  const stripped = tail.replace(/^[a-z]+-\d+-(\d+[a-z]?-)?/i, "").replace(/-/g, " ").trim();
  return stripped || tail;
}

/**
 * Append an EXISTING branch to a live stack as a new tip slice — the inverse
 * of `splitStack`'s reshape, and the registration path for "I `wt new --base
 * <tip>`'d a branch on top of the stack, now track it". Purely additive
 * (existing slices untouched), which is what lets it work on a materialized
 * stack — the re-ingest path refuses those wholesale.
 *
 * Never creates a branch or worktree (`wt new` owns that); errors when the
 * branch doesn't exist. DOES ensure the slice has a PR: adopts an open one,
 * else pushes + opens a draft PR against the parent. That isn't scope creep —
 * `validateStackManifest` hard-rejects `open` without a `pr`, and a `planned`
 * slice would later be re-materialized by `applyStack` from the HOLISTIC
 * branch, clobbering an externally-authored branch's content. PR-or-create is
 * what keeps `apply` permanently away from this slice.
 *
 * The squash-safe anchor (`baseSha`) is recorded as `merge-base(branch,
 * parent)` — not the parent's tip, which may have advanced since the fork and
 * wouldn't be an ancestor of the branch.
 */
export async function addSliceToStack(
  stackId: string,
  branch: string,
  opts: { onto?: string; title?: string },
  onLog: Logger,
): Promise<AddSliceResult> {
  const lock = await acquireStackLock("add");
  if (!lock) return { ok: false, error: STACK_BUSY };
  try {
    return await addSliceToStackLocked(stackId, branch, opts, onLog);
  } finally {
    lock.release();
  }
}

async function addSliceToStackLocked(
  stackId: string,
  branch: string,
  opts: { onto?: string; title?: string },
  onLog: Logger,
): Promise<AddSliceResult> {
  const manifest = getStackManifest(stackId);
  if (!manifest) return { ok: false, error: `no stack manifest: ${stackId}` };
  const owner = findStackIdByBranch(branch);
  if (owner) {
    return { ok: false, error: `${branch} is already tracked by stack ${owner}` };
  }
  if (!(await branchExists(branch))) {
    return {
      ok: false,
      error: `branch ${branch} not found (local or origin) — \`add\` registers an existing branch; create it first (e.g. \`wt new … --base <parentTip>\`)`,
    };
  }

  // Resolve the parent: `--onto` names a sibling slice (by id or branch) or
  // trunk (a new parallel lane root); default is the stack tip — the
  // highest-ordinal live slice.
  let parentSlice: StackSlice | null = null;
  if (opts.onto && opts.onto !== config.branch.base) {
    parentSlice =
      manifest.slices.find((s) => s.id === opts.onto || s.branch === opts.onto) ?? null;
    if (!parentSlice) {
      return {
        ok: false,
        error: `--onto ${opts.onto} matches no slice in ${stackId} (pass a slice id, a slice branch, or ${config.branch.base})`,
      };
    }
    if (parentSlice.status === "merged") {
      return {
        ok: false,
        error: `slice ${parentSlice.id} is merged — stack on a live slice, or --onto ${config.branch.base}`,
      };
    }
  } else if (!opts.onto) {
    // Prefer the fork base `wt new --base` recorded for this branch's
    // worktree, when it names a live slice of this stack — that's the
    // actual parent the branch was cut from. Fall back to the stack tip.
    const recorded = readWtState().slugs[dirSlug(branch)]?.baseBranch;
    const recordedSlice = recorded
      ? manifest.slices.find(
          (s) => (s.branch === recorded || s.id === recorded) && s.status !== "merged",
        )
      : undefined;
    if (recordedSlice) {
      parentSlice = recordedSlice;
      onLog(`parent ${recordedSlice.id} (${recordedSlice.branch}) from the recorded fork base`);
    } else {
      parentSlice =
        manifest.slices
          .filter((s) => s.status !== "merged")
          .sort((a, b) => b.ordinal - a.ordinal)[0] ?? null;
    }
    if (!parentSlice) {
      return {
        ok: false,
        error: `stack ${stackId} has no live slices — pass --onto ${config.branch.base} to root a new lane`,
      };
    }
  }
  const parentBranch = parentSlice ? parentSlice.branch : config.branch.base;

  const branchRef = await localOrOriginRef(branch);
  const parentRef = parentSlice
    ? await localOrOriginRef(parentBranch)
    : `origin/${config.branch.base}`;

  // Anchor = the cut point the branch's own commits sit on.
  const mb = await gitRun(["merge-base", branchRef, parentRef]);
  const baseSha = mb.stdout.trim();
  if (mb.exitCode !== 0 || !baseSha) {
    return {
      ok: false,
      error: `${branch} shares no history with ${parentBranch} — wrong parent? (--onto)`,
    };
  }

  // File partition = the branch's own diff vs the anchor. Doubles as the
  // empty-slice guard (nothing to review → nothing to track).
  const diff = await gitRun(["diff", "--name-only", `${baseSha}..${branchRef}`]);
  const files = diff.stdout.trim().split("\n").filter(Boolean);
  if (diff.exitCode !== 0 || files.length === 0) {
    return { ok: false, error: `${branch} has no changes on top of ${parentBranch} (empty slice)` };
  }

  // Ensure the PR. A CLOSED PR is not adopted — gh happily opens a fresh one
  // on the same branch, and the closed PR wasn't tracking this work anyway.
  let prNumber: number;
  let prAction: "adopted" | "created";
  let title = opts.title ?? "";
  const existing = await viewPrInfo(branch);
  if (existing?.state === "MERGED") {
    return {
      ok: false,
      error: `PR #${existing.number} for ${branch} is already merged — nothing left to stack`,
    };
  }
  if (existing && existing.state === "OPEN") {
    prNumber = existing.number;
    prAction = "adopted";
    title = title || existing.title;
    onLog(`adopted existing PR #${existing.number}`);
  } else {
    if (!(await originBranchExists(branch))) {
      const push = await gitRun(["push", "-u", "origin", branch]);
      if (push.exitCode !== 0) {
        return { ok: false, error: `push ${branch}: ${(push.stderr || push.stdout).trim()}` };
      }
      onLog(`pushed ${branch}`);
    }
    title = title || titleFromBranch(branch);
    const pr = await createDraftPr({
      cwd: config.paths.mainClone,
      head: branch,
      base: parentBranch,
      title,
      body: `Stacked on \`${parentBranch}\`.`,
    });
    if (!pr.ok) return { ok: false, error: `pr create ${branch}: ${pr.error}` };
    prNumber = pr.number;
    prAction = "created";
    onLog(`opened draft PR #${pr.number} (base ${parentBranch})`);
  }

  const maxOrdinal = Math.max(0, ...manifest.slices.map((s) => s.ordinal));
  const ids = new Set(manifest.slices.map((s) => s.id));
  let n = maxOrdinal + 1;
  while (ids.has(`s${n}`)) n++;

  const slice: StackSlice = {
    id: `s${n}`,
    ordinal: maxOrdinal + 1,
    title: title || titleFromBranch(branch),
    branch,
    base: parentSlice ? parentSlice.id : config.branch.base,
    dependsOn: parentSlice ? [parentSlice.id] : [],
    files,
    pr: prNumber,
    status: "open",
    oversized: false,
    baseSha,
  };

  const next: StackManifest = { ...manifest, slices: [...manifest.slices, slice] };
  const v = validateStackManifest(next);
  if (!v.ok) {
    return { ok: false, error: `manifest invalid after add:\n  ${v.errors.join("\n  ")}` };
  }
  putStackManifest(v.manifest);

  // An adopted PR may target the wrong base (e.g. trunk, because `gh pr
  // create` defaulted there) — align it with the manifest like replay does.
  if (prAction === "adopted") await retargetIfNeeded(slice, parentBranch, onLog);

  // The branch is a tracked slice now — the manifest owns its parent, so
  // the `wt new --base` fork-base hint is superseded. (Stack lock is held;
  // the wtstate flock nests inside it, consistent with every other mutator.)
  setSlugBase(dirSlug(branch), null);

  log.info("added slice to stack", { stackId, slice: slice.id, branch, parentBranch, prNumber });
  return { ok: true, slice, parentBranch, prAction };
}

// ---------- backup pruning ----------

export type PruneBackupsResult = { deleted: string[]; kept: string[] };

/**
 * Delete restack backup branches (`backup/restack-*` and the retired stack
 * CLI's `backup/stack-sync-*`) older than `olderThanDays` (0 = all of them).
 * Backups exist to recover an in-flight conflict bail; once a slice replays
 * clean the engine prunes its own, but conflict leftovers and pre-pruning
 * history pile up — this is the manual sweep. `git branch -D` doesn't destroy
 * commits; everything stays reachable via the reflog. Refs under `backup/`
 * that don't match a known naming scheme are left alone.
 */
export async function pruneStackBackups(
  olderThanDays: number,
  onLog: Logger,
): Promise<PruneBackupsResult> {
  const r = await gitRun(["for-each-ref", "--format=%(refname:short)", "refs/heads/backup/"]);
  const deleted: string[] = [];
  const kept: string[] = [];
  if (r.exitCode !== 0) return { deleted, kept };
  const cutoff = Date.now() - olderThanDays * 86_400_000;
  for (const ref of r.stdout.split("\n").map((l) => l.trim()).filter(Boolean)) {
    if (backupBranchOwner(ref) === null) {
      kept.push(ref);
      continue;
    }
    const ts = backupTimestamp(ref);
    if (ts === null || ts > cutoff) {
      kept.push(ref);
      continue;
    }
    const del = await gitRun(["branch", "-D", ref]);
    if (del.exitCode === 0) {
      deleted.push(ref);
      onLog(`  deleted ${ref}`);
    } else {
      kept.push(ref);
      onLog(`  could not delete ${ref}: ${(del.stderr || del.stdout).trim()}`);
    }
  }
  return { deleted, kept };
}
