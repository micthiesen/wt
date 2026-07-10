/**
 * Gate + reconstruction tests for the partial-file coverage logic, focused on
 * the FIX 2 scenario (ENG-5238 re-split post-mortem): a single file hunk-split
 * between a MERGED slice and an OPEN slice that don't form a dependency chain.
 *
 * The merged slice's hunk is already in trunk/base, so the gate must treat it
 * as covered-by-base (not "unassigned") and must NOT require the open slice to
 * descend from it. Reconstruction must still fold the merged hunk into the
 * open slice's ABSOLUTE content, or the commit would silently revert a landed
 * hunk when it diffs against the trunk parent that already contains it.
 *
 * Uses real `git diff` output (like `hunks.test.ts`) so the hunk ids are the
 * production content-hashes, not hand-rolled stand-ins.
 */
import { afterAll, expect, test } from "bun:test";

import { existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  ancestorOwnedHunks,
  isAdoptablePr,
  parseNameStatus,
  resolveAnchor,
  validateFileCoverage,
  validatePartialCoverage,
} from "./stack-ops.ts";
import { gitRun } from "./git.ts";
import { fileHunks } from "./stack-ops/hunks.ts";
import { transitiveAncestors } from "./stack-layout.ts";
import type { StackManifest, StackSlice } from "./wtstate.ts";

const dirs: string[] = [];
afterAll(() => {
  for (const d of dirs) rmSync(d, { recursive: true, force: true });
});

function git(cwd: string, args: string[]): string {
  const r = Bun.spawnSync(["git", ...args], {
    cwd,
    env: {
      ...process.env,
      GIT_CONFIG_GLOBAL: "/dev/null",
      GIT_CONFIG_SYSTEM: "/dev/null",
      GIT_AUTHOR_NAME: "t",
      GIT_AUTHOR_EMAIL: "t@t",
      GIT_COMMITTER_NAME: "t",
      GIT_COMMITTER_EMAIL: "t@t",
    },
  });
  if (r.exitCode !== 0) {
    throw new Error(`git ${args.join(" ")}: ${new TextDecoder().decode(r.stderr)}`);
  }
  return new TextDecoder().decode(r.stdout);
}

const lines = (...xs: string[]) => xs.map((x) => `${x}\n`).join("");

/** A repo with a `base` commit and a `holistic` branch carrying the two edits. */
function scenario(
  baseFiles: Record<string, string>,
  holisticFiles: Record<string, string>,
): { dir: string; base: string } {
  const dir = mkdtempSync(join(tmpdir(), "wt-stackops-test-"));
  dirs.push(dir);
  git(dir, ["init", "-q", "-b", "main"]);
  for (const [f, c] of Object.entries(baseFiles)) writeFileSync(join(dir, f), c);
  git(dir, ["add", "-A"]);
  git(dir, ["commit", "-q", "--allow-empty", "-m", "base"]);
  const base = git(dir, ["rev-parse", "HEAD"]).trim();
  git(dir, ["checkout", "-q", "-b", "holistic"]);
  for (const [f, c] of Object.entries(holisticFiles)) writeFileSync(join(dir, f), c);
  git(dir, ["add", "-A"]);
  git(dir, ["commit", "-q", "-m", "holistic"]);
  return { dir, base };
}

function manifest(slices: StackSlice[]): StackManifest {
  return {
    stackId: "eng-5238",
    issue: "ENG-5238",
    holisticBranch: "holistic",
    holisticSlug: "eng-5238",
    limits: { files: 3, prodLines: 150, hard: false },
    engine: "stack",
    slices,
  };
}

function slice(over: Partial<StackSlice> & Pick<StackSlice, "id">): StackSlice {
  return {
    ordinal: 1,
    title: over.id,
    branch: `michael/eng-5238-${over.id}`,
    base: "main",
    dependsOn: [],
    files: [],
    pr: null,
    status: "planned",
    oversized: false,
    ...over,
  };
}

/** The two separated hunk ids of the shared file, in diff order. */
async function twoHunks(dir: string, base: string, file: string): Promise<[string, string]> {
  const fd = await fileHunks(dir, base, "holistic", file);
  expect(fd.hunks).toHaveLength(2);
  return [fd.hunks[0]!.id, fd.hunks[1]!.id];
}

// A file with two edits separated by enough context to stay distinct hunks.
const FILE = "useChatThread.ts";
const BASE_BODY = lines("h", "1", "2", "3", "4", "5", "6", "7", "8", "9", "t");
const HOL_BODY = lines("H", "1", "2", "3", "4", "5", "6", "7", "8", "9", "T");

test("merged + open co-owners (no chain) pass the coverage gate", async () => {
  const { dir, base } = scenario({ [FILE]: BASE_BODY }, { [FILE]: HOL_BODY });
  const [h1, h2] = await twoHunks(dir, base, FILE);
  // s1 owns hunk 1 and has MERGED; s4 owns hunk 2, OPEN, and does NOT depend
  // on s1 (parallel) — the exact useChatThread.ts shape from the post-mortem.
  const m = manifest([
    slice({ id: "s1", ordinal: 1, status: "merged", pr: 1, partials: [{ file: FILE, hunks: [h1] }] }),
    slice({ id: "s4", ordinal: 2, status: "open", pr: 4, partials: [{ file: FILE, hunks: [h2] }] }),
  ]);
  const baseBySource = new Map([["holistic", base]]);
  const err = await validatePartialCoverage(m, dir, transitiveAncestors(m.slices), baseBySource);
  expect(err).toBeNull();
});

test("merged hunk absorbed into an advanced base still passes the gate", async () => {
  // The FIX-1 regression: `base` is recomputed live each apply, and the source
  // branch gets rebased onto post-merge trunk (`/restack`). Once the fork point
  // advances PAST the merged hunk, that hunk is in base and DROPS OUT of
  // `fileHunks(base, source)` — so the merged slice's content-hashed id is no
  // longer in `known`. That's the expected end state (the hunk migrated into
  // base), not drift. The stale check must NOT flag it.
  const { dir, base } = scenario({ [FILE]: BASE_BODY }, { [FILE]: HOL_BODY });
  const [h1, h2] = await twoHunks(dir, base, FILE);
  // Build an ADVANCED base: a commit off `base` that already carries h1's edit
  // (the merged slice's content), as if it had landed and trunk moved past it.
  // Diffing this advanced base against holistic shows ONLY h2 — h1 is absorbed.
  git(dir, ["checkout", "-q", "-b", "advanced", base]);
  writeFileSync(join(dir, FILE), lines("H", "1", "2", "3", "4", "5", "6", "7", "8", "9", "t"));
  git(dir, ["add", "-A"]);
  git(dir, ["commit", "-q", "-m", "advanced base (h1 landed)"]);
  const advancedBase = git(dir, ["rev-parse", "HEAD"]).trim();
  git(dir, ["checkout", "-q", "holistic"]);
  // Confirm the advanced base only diffs by h2 (h1 is gone from the diff).
  const fd = await fileHunks(dir, advancedBase, "holistic", FILE);
  expect(fd.hunks.map((h) => h.id)).toEqual([h2]);
  const m = manifest([
    slice({ id: "s1", ordinal: 1, status: "merged", pr: 1, partials: [{ file: FILE, hunks: [h1] }] }),
    slice({ id: "s4", ordinal: 2, status: "open", pr: 4, partials: [{ file: FILE, hunks: [h2] }] }),
  ]);
  // Pin the resolved base to the advanced SHA (what a post-merge re-apply sees).
  const err = await validatePartialCoverage(
    m,
    dir,
    transitiveAncestors(m.slices),
    new Map([["holistic", advancedBase]]),
  );
  expect(err).toBeNull();
});

test("dropping the merged owner makes the gate flag the landed hunk as unassigned", async () => {
  // Sanity: without the merged co-owner the gate SHOULD complain — proves the
  // pass above is the merged-aware path, not a no-op.
  const { dir, base } = scenario({ [FILE]: BASE_BODY }, { [FILE]: HOL_BODY });
  const [, h2] = await twoHunks(dir, base, FILE);
  const m = manifest([
    slice({ id: "s4", ordinal: 1, status: "open", pr: 4, partials: [{ file: FILE, hunks: [h2] }] }),
  ]);
  const err = await validatePartialCoverage(m, dir, transitiveAncestors(m.slices), new Map([["holistic", base]]));
  expect(err).toContain("unassigned");
});

test("two LIVE parallel owners still fail the chain gate", async () => {
  // The merged-exclusion must not weaken the chain check for genuinely-live
  // parallel owners (each would carry half the file, no tip the whole).
  const { dir, base } = scenario({ [FILE]: BASE_BODY }, { [FILE]: HOL_BODY });
  const [h1, h2] = await twoHunks(dir, base, FILE);
  const m = manifest([
    slice({ id: "s1", ordinal: 1, status: "open", pr: 1, partials: [{ file: FILE, hunks: [h1] }] }),
    slice({ id: "s4", ordinal: 2, status: "open", pr: 4, partials: [{ file: FILE, hunks: [h2] }] }),
  ]);
  const err = await validatePartialCoverage(m, dir, transitiveAncestors(m.slices), new Map([["holistic", base]]));
  expect(err).toContain("dependency chain");
});

test("open slice reconstruction folds in the merged co-owner's landed hunk", async () => {
  // The reconstruction owned-set for the open slice must include the merged
  // slice's hunk even though it's NOT a dependsOn-ancestor — otherwise the
  // committed absolute content omits it and reverts the landed hunk.
  const { dir, base } = scenario({ [FILE]: BASE_BODY }, { [FILE]: HOL_BODY });
  const [h1, h2] = await twoHunks(dir, base, FILE);
  const s1 = slice({ id: "s1", ordinal: 1, status: "merged", pr: 1, partials: [{ file: FILE, hunks: [h1] }] });
  const s4 = slice({ id: "s4", ordinal: 2, status: "open", pr: 4, partials: [{ file: FILE, hunks: [h2] }] });
  const m = manifest([s1, s4]);
  const owned = ancestorOwnedHunks(m, s4, transitiveAncestors(m.slices));
  // h1 (the merged, non-ancestor hunk) is in s4's already-in-base set; h2 is
  // s4's own and is applied separately by materialize, not via this map.
  expect(owned.get(FILE)).toEqual(new Set([h1]));
});

// ---- FIX 4: applyStack adopts only OPEN/MERGED PRs, never CLOSED ----

test("adopt guard: OPEN and MERGED adoptable, CLOSED is not", () => {
  // A CLOSED PR on a reused branch name (a superseded re-split slice) must
  // fall through to a fresh push + create instead of being adopted as the
  // slice's PR. OPEN = idempotent re-run; MERGED = already landed.
  expect(isAdoptablePr("OPEN")).toBe(true);
  expect(isAdoptablePr("MERGED")).toBe(true);
  expect(isAdoptablePr("CLOSED")).toBe(false);
});

test("reused branch (closed PR) is reset onto parent before materialize", async () => {
  // FIX-2 at the git level: when `applyStack` falls through to create on a
  // branch that already exists (its prior PR was CLOSED, name reused), the
  // create path checks out the STALE tip and ignores `base`. Without the
  // hard-reset the engine adds, `baseSha = HEAD` would be the stale tip and
  // the slice would be committed on top of stale content. Reset onto the
  // fresh parent fixes both: HEAD == parent tip, and a fresh commit diffs
  // ONLY the slice's content against the parent.
  const dir = mkdtempSync(join(tmpdir(), "wt-stackops-reset-"));
  dirs.push(dir);
  git(dir, ["init", "-q", "-b", "main"]);
  writeFileSync(join(dir, "a.ts"), "base\n");
  git(dir, ["add", "-A"]);
  git(dir, ["commit", "-q", "-m", "base"]);
  // The fresh parent advances past the fork point (e.g. a sibling slice landed).
  writeFileSync(join(dir, "parent.ts"), "parent\n");
  git(dir, ["add", "-A"]);
  git(dir, ["commit", "-q", "-m", "parent"]);
  const parentRef = git(dir, ["rev-parse", "HEAD"]).trim();
  // A reused branch sitting at a STALE tip (the superseded slice's old work).
  git(dir, ["checkout", "-q", "-b", "michael/eng-5238-s4", "main"]);
  writeFileSync(join(dir, "stale.ts"), "stale superseded content\n");
  git(dir, ["add", "-A"]);
  git(dir, ["commit", "-q", "-m", "stale superseded slice"]);

  // The production fall-through path: reset the reused branch onto parentRef.
  const reset = await gitRun(["reset", "--hard", parentRef], dir);
  expect(reset.exitCode).toBe(0);
  // HEAD now == the fresh parent (correct squash-safe anchor).
  expect(git(dir, ["rev-parse", "HEAD"]).trim()).toBe(parentRef);
  // Stale content is gone; parent content is present.
  expect(existsSync(join(dir, "stale.ts"))).toBe(false);
  expect(existsSync(join(dir, "parent.ts"))).toBe(true);
});

// ── whole-file coverage gate (validateFileCoverage / parseNameStatus) ──

test("parseNameStatus maps a rename to both halves and others to null", () => {
  const out = lines("M\tkept.ts", "A\tadded.ts", "D\tgone.ts", "R100\told.ts\tnew.ts");
  const m = parseNameStatus(out);
  expect(m.get("kept.ts")).toBeNull();
  expect(m.get("added.ts")).toBeNull();
  expect(m.get("gone.ts")).toBeNull();
  expect(m.get("old.ts")).toBe("new.ts");
  expect(m.get("new.ts")).toBe("old.ts");
  expect(m.size).toBe(5);
});

/** A `holistic` branch that renames `old.ts`→`renamed.ts` and adds `brandnew.ts`. */
function renameScenario(): { dir: string; base: string } {
  const dir = mkdtempSync(join(tmpdir(), "wt-stackops-fc-"));
  dirs.push(dir);
  git(dir, ["init", "-q", "-b", "main"]);
  writeFileSync(join(dir, "old.ts"), BASE_BODY);
  git(dir, ["add", "-A"]);
  git(dir, ["commit", "-q", "-m", "base"]);
  const base = git(dir, ["rev-parse", "HEAD"]).trim();
  git(dir, ["checkout", "-q", "-b", "holistic"]);
  git(dir, ["mv", "old.ts", "renamed.ts"]);
  writeFileSync(join(dir, "brandnew.ts"), "added\n");
  git(dir, ["add", "-A"]);
  git(dir, ["commit", "-q", "-m", "holistic"]);
  return { dir, base };
}

test("validateFileCoverage passes when every changed path (both rename halves) is claimed", async () => {
  const { dir, base } = renameScenario();
  const m = manifest([slice({ id: "s1", files: ["old.ts", "renamed.ts", "brandnew.ts"] })]);
  const err = await validateFileCoverage(m, dir, new Map([["holistic", base]]));
  expect(err).toBeNull();
});

test("validateFileCoverage flags a rename's unclaimed delete-half, naming the counterpart owner", async () => {
  const { dir, base } = renameScenario();
  // Claims the rename NEW half + the add, but forgets the OLD (delete) half.
  const m = manifest([slice({ id: "s2", files: ["renamed.ts", "brandnew.ts"] })]);
  const err = await validateFileCoverage(m, dir, new Map([["holistic", base]]));
  expect(err).toContain("old.ts");
  expect(err).toContain("rename of renamed.ts");
  expect(err).toContain("slice s2");
});

test("validateFileCoverage flags a plain unclaimed addition", async () => {
  const { dir, base } = renameScenario();
  // Forgets `brandnew.ts` entirely (a non-rename unclaimed path).
  const m = manifest([slice({ id: "s1", files: ["old.ts", "renamed.ts"] })]);
  const err = await validateFileCoverage(m, dir, new Map([["holistic", base]]));
  expect(err).toContain("brandnew.ts");
});

// resolveAnchor: the squash-safe replay cut point survives a hand-rebase. Two
// stale-anchor shapes a bare `--is-ancestor` guard couldn't tell apart.

test("resolveAnchor uses the live merge-base when a slice was rebased onto newer trunk", async () => {
  // eng-5244 reproduction: the slice was hand-rebased onto newer trunk, so the
  // OLD baseSha (p1) is STILL an ancestor of the branch — the naive guard trusts
  // it and replays all of trunk's history. The real fork point advanced to p2.
  const dir = mkdtempSync(join(tmpdir(), "wt-anchor-test-"));
  dirs.push(dir);
  git(dir, ["init", "-q", "-b", "main"]);
  git(dir, ["commit", "-q", "--allow-empty", "-m", "M0"]);
  git(dir, ["checkout", "-q", "-b", "p"]);
  git(dir, ["commit", "-q", "--allow-empty", "-m", "P1"]);
  const p1 = git(dir, ["rev-parse", "HEAD"]).trim();
  git(dir, ["commit", "-q", "--allow-empty", "-m", "P2"]);
  const p2 = git(dir, ["rev-parse", "HEAD"]).trim();
  git(dir, ["checkout", "-q", "-b", "c"]); // c built on the advanced parent tip
  git(dir, ["commit", "-q", "--allow-empty", "-m", "C"]);

  const s = slice({ id: "c", branch: "c", base: "p", baseSha: p1, status: "open" });
  const anchor = await resolveAnchor(s, new Map(), "main", dir);
  expect(anchor).toBe(p2); // the true fork point, so only C replays
  expect(anchor).not.toBe(p1); // not the stale stored anchor
});

test("resolveAnchor keeps baseSha when the live merge-base is older (squash-merged parent)", async () => {
  // The healthy squash case: the parent squash-merged into the integration
  // branch as one commit, so merge-base(child, parent) drops to M0 — BELOW the
  // recorded baseSha. baseSha must stand, or the squashed parent's commits get
  // re-applied.
  const dir = mkdtempSync(join(tmpdir(), "wt-anchor-test-"));
  dirs.push(dir);
  git(dir, ["init", "-q", "-b", "main"]);
  git(dir, ["commit", "-q", "--allow-empty", "-m", "M0"]);
  const m0 = git(dir, ["rev-parse", "HEAD"]).trim();
  git(dir, ["checkout", "-q", "-b", "p"]);
  git(dir, ["commit", "-q", "--allow-empty", "-m", "P1"]);
  const p1 = git(dir, ["rev-parse", "HEAD"]).trim();
  git(dir, ["checkout", "-q", "-b", "c"]); // child built on the parent tip
  git(dir, ["commit", "-q", "--allow-empty", "-m", "C"]);
  git(dir, ["checkout", "-q", "-b", "released", "main"]);
  git(dir, ["commit", "-q", "--allow-empty", "-m", "squash of p"]);

  const s = slice({ id: "c", branch: "c", base: "released", baseSha: p1, status: "open" });
  const anchor = await resolveAnchor(s, new Map(), "main", dir);
  expect(anchor).toBe(p1); // squash-safe anchor preserved
  expect(anchor).not.toBe(m0);
});
