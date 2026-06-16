/**
 * Hunk-level partition support for stack slices. The atomic-file rule
 * (a changed file belongs wholly to one slice) is what `materializeSliceCommit`
 * enforces with `git checkout <holistic> -- <file>`. When a single file's
 * changes legitimately serve different slices (e.g. a fixture stub that
 * belongs in an early "add the field" slice and the behavior that reads it
 * in a later slice), that rule forces them together and collapses an
 * otherwise-clean stack.
 *
 * This module makes a file partitionable by hunk. The holistic diff for a
 * file decomposes into hunks with STABLE ids (content-hashed, independent of
 * line numbers so they survive incremental rebuilds). A slice records which
 * hunk ids it owns; `reconstructFile` rebuilds the exact intermediate content
 * a slice's commit should carry by applying only the owned subset to the
 * base version. Reconstruction is pure text replay — no `git apply`, no fuzz,
 * no conflict at materialize time. The only place per-hunk fragility can bite
 * is replay (rebasing an already-authored slice onto a moved parent), which
 * keeps its existing conflict → bail → `/restack` path unchanged.
 *
 * The id scheme is the contract `/split` must reproduce: it lists canonical
 * ids via `wt stack hunks` rather than re-implementing the hash.
 */
import { createHash } from "node:crypto";

import { config } from "./config.ts";
import { gitRun } from "./git.ts";

export type HunkLineKind = " " | "+" | "-";

export type HunkLine = { kind: HunkLineKind; text: string };

export type ParsedHunk = {
  /** Content-hash id, stable across line-number shifts. `<hash>` or `<hash>~N` for duplicate bodies. */
  id: string;
  header: string;
  oldStart: number;
  oldCount: number;
  newStart: number;
  newCount: number;
  lines: HunkLine[];
  /** The hunk's NEW side ends with a `\ No newline at end of file` marker. */
  newEndsNoEol: boolean;
  /** The hunk's OLD side ends with a no-newline marker. */
  oldEndsNoEol: boolean;
};

export type FileDiff = {
  file: string;
  /** Lines before the first `@@` (the `diff --git` / `index` / `---` / `+++` header). */
  preamble: string[];
  hunks: ParsedHunk[];
  /** Git reported the file as binary — it can't be hunk-split. */
  binary: boolean;
};

const HUNK_HEADER = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/;

/** Stable id for a hunk: first 12 hex of sha256 over its body lines (markers included). */
function bodyHash(lines: HunkLine[]): string {
  const h = createHash("sha256");
  for (const ln of lines) h.update(`${ln.kind}${ln.text}\n`);
  return h.digest("hex").slice(0, 12);
}

/**
 * Parse the `git diff` output for ONE file into its hunks. Duplicate hunk
 * bodies (the same change in two places) get a `~N` occurrence suffix in
 * file order so every id is unique and deterministic.
 */
export function parseFileDiff(file: string, raw: string): FileDiff {
  const allLines = raw.split("\n");
  const preamble: string[] = [];
  const hunks: ParsedHunk[] = [];
  let binary = false;
  let i = 0;
  for (; i < allLines.length; i++) {
    const line = allLines[i]!;
    if (HUNK_HEADER.test(line)) break;
    if (line.startsWith("Binary files ") || line.startsWith("GIT binary patch")) binary = true;
    preamble.push(line);
  }
  while (i < allLines.length) {
    const header = allLines[i]!;
    const m = header.match(HUNK_HEADER);
    if (!m) {
      i++;
      continue;
    }
    const oldStart = Number.parseInt(m[1]!, 10);
    const oldCount = m[2] === undefined ? 1 : Number.parseInt(m[2], 10);
    const newStart = Number.parseInt(m[3]!, 10);
    const newCount = m[4] === undefined ? 1 : Number.parseInt(m[4], 10);
    const lines: HunkLine[] = [];
    let newEndsNoEol = false;
    let oldEndsNoEol = false;
    i++;
    for (; i < allLines.length; i++) {
      const l = allLines[i]!;
      if (HUNK_HEADER.test(l)) break;
      if (l.startsWith("\\")) {
        // "\ No newline at end of file" applies to the immediately preceding
        // line — flag whichever side that line belonged to.
        const prev = lines[lines.length - 1];
        if (prev) {
          if (prev.kind === "+") newEndsNoEol = true;
          else if (prev.kind === "-") oldEndsNoEol = true;
          else {
            newEndsNoEol = true;
            oldEndsNoEol = true;
          }
        }
        continue;
      }
      const kind = l[0];
      if (kind === " " || kind === "+" || kind === "-") {
        lines.push({ kind, text: l.slice(1) });
      } else if (l === "") {
        // A real empty context line arrives as a single space (" "), not "".
        // A bare "" is the trailing artifact of `raw.split("\n")` — stop at it
        // when it's the last line; defensively skip any stray earlier one.
        if (i === allLines.length - 1) break;
        continue;
      } else {
        break;
      }
    }
    hunks.push({
      id: bodyHash(lines),
      header,
      oldStart,
      oldCount,
      newStart,
      newCount,
      lines,
      newEndsNoEol,
      oldEndsNoEol,
    });
  }
  // Disambiguate duplicate bodies deterministically in file order: identical
  // hunk bodies get a 0-based occurrence suffix (`<hash>~0`, `<hash>~1`, …).
  const seen = new Map<string, number>();
  const total = new Map<string, number>();
  for (const h of hunks) total.set(h.id, (total.get(h.id) ?? 0) + 1);
  for (const h of hunks) {
    if ((total.get(h.id) ?? 0) > 1) {
      const n = seen.get(h.id) ?? 0;
      seen.set(h.id, n + 1);
      h.id = `${h.id}~${n}`;
    }
  }
  return { file, preamble, hunks, binary };
}

/** Split raw file bytes into lines, remembering whether it ended with a newline. */
function splitContent(raw: string): { lines: string[]; endsWithNewline: boolean } {
  if (raw === "") return { lines: [], endsWithNewline: true };
  const endsWithNewline = raw.endsWith("\n");
  const body = endsWithNewline ? raw.slice(0, -1) : raw;
  return { lines: body.split("\n"), endsWithNewline };
}

/**
 * Reconstruct the exact content a slice's commit should carry for a partial
 * file: the base version with ONLY the owned subset of holistic hunks applied.
 * Unowned hunks leave the base region untouched. Pure text replay; hunks are
 * disjoint and processed in file order, so there's no apply fuzz.
 */
export function reconstructFile(
  baseRaw: string,
  hunks: ParsedHunk[],
  owned: ReadonlySet<string>,
): string {
  const { lines: baseLines, endsWithNewline: baseEol } = splitContent(baseRaw);
  const ordered = [...hunks].sort((a, b) => a.oldStart - b.oldStart);
  const out: string[] = [];
  let cursor = 0;
  // The hunk whose region reaches the base's end governs the trailing newline
  // (owned → its new side, unowned → the old side); default to the base's.
  let finalEol = baseEol;
  for (const h of ordered) {
    // `oldStart` is 1-based; a replacement region begins at `oldStart - 1`
    // (0-based), but a pure insertion (`oldCount === 0`) sits AFTER `oldStart`.
    const regionStart = h.oldCount === 0 ? h.oldStart : h.oldStart - 1;
    while (cursor < regionStart && cursor < baseLines.length) out.push(baseLines[cursor++]!);
    const reachesEnd = cursor + h.oldCount >= baseLines.length;
    if (owned.has(h.id)) {
      for (const ln of h.lines) {
        if (ln.kind === " " || ln.kind === "+") out.push(ln.text);
      }
      if (reachesEnd) finalEol = !h.newEndsNoEol;
    } else {
      for (let k = 0; k < h.oldCount && cursor + k < baseLines.length; k++) {
        out.push(baseLines[cursor + k]!);
      }
      if (reachesEnd) finalEol = !h.oldEndsNoEol;
    }
    cursor += h.oldCount;
  }
  while (cursor < baseLines.length) out.push(baseLines[cursor++]!);
  if (out.length === 0) return "";
  return out.join("\n") + (finalEol ? "\n" : "");
}

/** Added + removed line counts for a single hunk. */
export function hunkLineCounts(h: ParsedHunk): { added: number; removed: number } {
  let added = 0;
  let removed = 0;
  for (const ln of h.lines) {
    if (ln.kind === "+") added++;
    else if (ln.kind === "-") removed++;
  }
  return { added, removed };
}

/** Added + removed lines across the owned subset of a file's hunks (for sizing). */
export function countHunkLines(hunks: ParsedHunk[], owned: ReadonlySet<string>): number {
  let n = 0;
  for (const h of hunks) {
    if (!owned.has(h.id)) continue;
    const { added, removed } = hunkLineCounts(h);
    n += added + removed;
  }
  return n;
}

/**
 * The base ref a holistic diff is partitioned against: the fork point of
 * `holisticBranch` from trunk. `/split` and `materializeSliceCommit` MUST use
 * the same base so hunk ids line up.
 */
export async function holisticBase(cwd: string, holisticBranch: string): Promise<string> {
  const trunk = `origin/${config.branch.base}`;
  const mb = await gitRun(["merge-base", trunk, holisticBranch], cwd);
  const sha = mb.stdout.trim();
  return mb.exitCode === 0 && sha ? sha : trunk;
}

/** Compute + parse the holistic diff hunks for one file. */
export async function fileHunks(
  cwd: string,
  base: string,
  holisticBranch: string,
  file: string,
): Promise<FileDiff> {
  const r = await gitRun(
    ["-c", "core.quotePath=false", "diff", "--no-color", base, holisticBranch, "--", file],
    cwd,
  );
  return parseFileDiff(file, r.stdout);
}

/** The base-side content of a file (empty string when the file is new on `base`). */
export async function baseContent(cwd: string, base: string, file: string): Promise<string> {
  const r = await gitRun(["show", `${base}:${file}`], cwd);
  return r.exitCode === 0 ? r.stdout : "";
}
