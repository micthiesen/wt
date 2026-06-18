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

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { ancestorOwnedHunks, isAdoptablePr, validatePartialCoverage } from "./stack-ops.ts";
import { fileHunks } from "./hunks.ts";
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
