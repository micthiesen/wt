import { mkdir } from "node:fs/promises";
import { dirname, join } from "node:path";

import { config } from "../config.ts";
import {
  branchExists,
  gitQuiet,
  gitRun,
  localOrOriginRef,
  revParse,
} from "../git.ts";
import {
  baseContent,
  DEFAULT_HUNK_CONTEXT,
  fileHunks,
  holisticBase,
  reconstructFile,
} from "../hunks.ts";
import { createWorktree } from "../lifecycle.ts";
import {
  createDraftPr,
  viewPrInfo,
  type LivePrInfo,
} from "../github.ts";
import {
  isTrunkBase,
  resolveParentBranch,
  topoSortSlices,
  transitiveAncestors,
} from "../stack-layout.ts";
import { verifyStack } from "../stack-verify.ts";
import {
  getStackManifest,
  patchStackManifest,
  updateStackSlice,
  type StackManifest,
  type StackSlice,
} from "../wtstate.ts";
import { acquireStackLock, log, STACK_BUSY, type Logger } from "./shared.ts";

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
