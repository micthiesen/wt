import { config } from "../../../core/config.ts";
import { DEFAULT_HUNK_CONTEXT, fileHunks, holisticBase, hunkLineCounts } from "../../../core/hunks.ts";
import { getStackManifest } from "../../../core/wtstate.ts";
import { bold, cyan, dim, green, red } from "../../colors.ts";
import { stackIdFromCwd } from "./shared.ts";

/**
 * List the canonical hunk ids of a file's holistic diff, so `/split` can
 * assign hunks to slices without re-implementing the content-hash scheme.
 * The base is the holistic branch's fork point from trunk — the SAME base
 * `materializeSliceCommit` reconstructs against, so ids line up.
 */
export async function runHunks(rest: string[]): Promise<number> {
  let holistic = "";
  let json = false;
  let context: number | undefined;
  const files: string[] = [];
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i]!;
    if (a === "--holistic") {
      const v = rest[++i];
      if (v === undefined || v.startsWith("--")) {
        console.error(red("--holistic needs a branch name"));
        return 2;
      }
      holistic = v;
    } else if (a === "--unified" || a === "-U") {
      const v = rest[++i];
      // Strict decimal only — `Number()` would quietly accept "", "0x4", "1e1".
      if (v === undefined || !/^\d+$/.test(v)) {
        console.error(red("--unified needs a non-negative integer"));
        return 2;
      }
      context = Number(v);
    } else if (a === "--json") json = true;
    else files.push(a);
  }
  if (files.length === 0) {
    console.error(red("usage: wt stack hunks [--holistic <branch>] [--unified <n>] [--json] <file>..."));
    return 2;
  }
  // Default the context from the resolved stack's pinned `hunkContext` so a
  // bare listing matches what `apply` will reconstruct against; an explicit
  // --unified always wins.
  let manifestContext: number | undefined;
  if (!holistic) {
    const stackId = await stackIdFromCwd();
    const manifest = stackId ? getStackManifest(stackId) : null;
    if (manifest?.holisticBranch) holistic = manifest.holisticBranch;
    manifestContext = manifest?.hunkContext;
  }
  if (!holistic) {
    console.error(red("no --holistic branch given and none resolvable from the current branch's stack"));
    return 2;
  }
  const effectiveContext = context ?? manifestContext ?? DEFAULT_HUNK_CONTEXT;
  const cwd = config.paths.mainClone;
  const base = await holisticBase(cwd, holistic);
  type HunkInfo = { id: string; header: string; added: number; removed: number };
  const out: Array<{ file: string; base: string; binary: boolean; hunks: HunkInfo[] }> = [];
  for (const file of files) {
    const fd = await fileHunks(cwd, base, holistic, file, effectiveContext);
    const hunks: HunkInfo[] = fd.hunks.map((h) => {
      const { added, removed } = hunkLineCounts(h);
      return { id: h.id, header: h.header, added, removed };
    });
    if (json) {
      out.push({ file, base, binary: fd.binary, hunks });
      continue;
    }
    if (fd.binary) {
      console.log(`${bold(file)} ${red("(binary — cannot hunk-split)")}`);
      continue;
    }
    if (hunks.length === 0) {
      console.log(`${bold(file)} ${dim("(no hunks)")}`);
      continue;
    }
    console.log(bold(file));
    for (const h of hunks) {
      console.log(`  ${cyan(h.id)}  ${green(`+${h.added}`)} ${red(`-${h.removed}`)}  ${dim(h.header)}`);
    }
  }
  if (json) console.log(JSON.stringify(out, null, 2));
  return 0;
}
