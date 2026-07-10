/**
 * Golden tests for the hunk engine — the correctness-critical core of
 * hunk-level stack slicing. `parseFileDiff` + `reconstructFile` do cursor
 * arithmetic over unified-diff hunks; a subtle off-by-one (insertion vs
 * replacement `regionStart`, no-newline-at-EOF, duplicate-body ids) doesn't
 * throw — it silently writes wrong content into a slice commit. These tests
 * pin the behavior against REAL `git diff` output (not hand-rolled patches),
 * so they catch a regression the way materialize would actually hit it.
 *
 * The load-bearing invariant, asserted across every scenario: reconstructing
 * with ALL hunks owned reproduces the holistic file byte-for-byte, and with
 * NONE owned reproduces the base — so coverage at the tip is exact.
 */
import { afterAll, expect, test } from "bun:test";

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  baseContent,
  countHunkLines,
  fileHunks,
  hunkLineCounts,
  reconstructFile,
} from "./hunks.ts";

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

/**
 * Build a throwaway repo with a `base` commit and a `holistic` branch, write
 * the two file states, and return the base sha + repo dir. `holistic` values
 * of `null` delete the file on the holistic side.
 */
function scenario(
  baseFiles: Record<string, string>,
  holisticFiles: Record<string, string | null>,
): { dir: string; base: string } {
  const dir = mkdtempSync(join(tmpdir(), "wt-hunks-test-"));
  dirs.push(dir);
  git(dir, ["init", "-q", "-b", "main"]);
  git(dir, ["commit", "-q", "--allow-empty", "-m", "root"]);
  for (const [f, c] of Object.entries(baseFiles)) writeFileSync(join(dir, f), c);
  git(dir, ["add", "-A"]);
  // --allow-empty so a new-file scenario (no base files) still gets a base commit.
  git(dir, ["commit", "-q", "--allow-empty", "-m", "base"]);
  const base = git(dir, ["rev-parse", "HEAD"]).trim();
  git(dir, ["checkout", "-q", "-b", "holistic"]);
  for (const [f, c] of Object.entries(holisticFiles)) {
    if (c === null) git(dir, ["rm", "-q", "--", f]);
    else writeFileSync(join(dir, f), c);
  }
  git(dir, ["add", "-A"]);
  git(dir, ["commit", "-q", "-m", "holistic"]);
  return { dir, base };
}

/** The holistic content of a file (empty string when deleted on holistic). */
function holisticContent(dir: string, file: string): string {
  const r = Bun.spawnSync(["git", "show", `holistic:${file}`], { cwd: dir });
  return r.exitCode === 0 ? new TextDecoder().decode(r.stdout) : "";
}

/**
 * Assert the framing invariant for one file: all-owned == holistic,
 * none-owned == base. Returns the parsed hunks for finer per-test assertions.
 */
async function checkFraming(dir: string, base: string, file: string, context = 3) {
  const fd = await fileHunks(dir, base, "holistic", file, context);
  const raw = await baseContent(dir, base, file);
  const all = new Set(fd.hunks.map((h) => h.id));
  expect(reconstructFile(raw, fd.hunks, all)).toBe(holisticContent(dir, file));
  expect(reconstructFile(raw, fd.hunks, new Set())).toBe(raw);
  return fd;
}

const lines = (...xs: string[]) => xs.map((x) => `${x}\n`).join("");

test("single replacement hunk: owned → holistic, unowned → base", async () => {
  const { dir, base } = scenario(
    { "a.txt": lines("alpha", "beta", "gamma") },
    { "a.txt": lines("alpha", "BETA", "gamma") },
  );
  const fd = await checkFraming(dir, base, "a.txt");
  expect(fd.hunks).toHaveLength(1);
});

test("two separated hunks: partial ownership picks one", async () => {
  // 7 lines of spacing keeps the two edits in distinct hunks at U3.
  const baseBody = lines("h", "1", "2", "3", "4", "5", "6", "7", "8", "9", "t");
  const holBody = lines("H", "1", "2", "3", "4", "5", "6", "7", "8", "9", "T");
  const { dir, base } = scenario({ "f.txt": baseBody }, { "f.txt": holBody });
  const fd = await checkFraming(dir, base, "f.txt");
  expect(fd.hunks).toHaveLength(2);
  const raw = await baseContent(dir, base, "f.txt");
  // Own only the first hunk: line 1 flips, last line stays at base.
  const first = reconstructFile(raw, fd.hunks, new Set([fd.hunks[0]!.id]));
  expect(first).toBe(lines("H", "1", "2", "3", "4", "5", "6", "7", "8", "9", "t"));
  const second = reconstructFile(raw, fd.hunks, new Set([fd.hunks[1]!.id]));
  expect(second).toBe(lines("h", "1", "2", "3", "4", "5", "6", "7", "8", "9", "T"));
});

test("pure insertion (oldCount 0) at top, middle, end", async () => {
  const { dir, base } = scenario(
    { "i.txt": lines("one", "two", "three") },
    { "i.txt": lines("TOP", "one", "two", "MID", "three", "END") },
  );
  await checkFraming(dir, base, "i.txt");
});

test("deletion of lines", async () => {
  const { dir, base } = scenario(
    { "d.txt": lines("k", "drop1", "drop2", "l", "m") },
    { "d.txt": lines("k", "l", "m") },
  );
  await checkFraming(dir, base, "d.txt");
});

test("new file: base absent, all-owned rebuilds it, none-owned is empty", async () => {
  const { dir, base } = scenario({}, { "new.txt": lines("fresh", "content") });
  const fd = await fileHunks(dir, base, "holistic", "new.txt");
  const raw = await baseContent(dir, base, "new.txt");
  expect(raw).toBe("");
  const all = new Set(fd.hunks.map((h) => h.id));
  expect(reconstructFile(raw, fd.hunks, all)).toBe(lines("fresh", "content"));
  expect(reconstructFile(raw, fd.hunks, new Set())).toBe("");
});

test("no newline at EOF on the holistic side is preserved", async () => {
  const { dir, base } = scenario(
    { "n.txt": lines("a", "b", "c") },
    { "n.txt": "a\nb\nCEE" }, // no trailing newline
  );
  const fd = await checkFraming(dir, base, "n.txt");
  const raw = await baseContent(dir, base, "n.txt");
  const all = new Set(fd.hunks.map((h) => h.id));
  expect(reconstructFile(raw, fd.hunks, all)).toBe("a\nb\nCEE");
});

test("no newline at EOF on the base side, holistic adds one", async () => {
  const { dir, base } = scenario(
    { "n2.txt": "a\nb\nc" }, // base has no trailing newline
    { "n2.txt": lines("a", "b", "C") },
  );
  await checkFraming(dir, base, "n2.txt");
});

test("duplicate hunk bodies get ~N suffixes and reconstruct independently", async () => {
  // Identical bodies need identical CONTEXT too, so this only triggers at U0
  // (zero-context bodies). The same single-line edit in two spots → `-X\n+Y`
  // twice → ~0 / ~1 disambiguation. At U3 the surrounding lines would differ
  // and the ids would already be distinct.
  const { dir, base } = scenario(
    { "dup.txt": lines("X", "a", "b", "c", "d", "e", "f", "X") },
    { "dup.txt": lines("Y", "a", "b", "c", "d", "e", "f", "Y") },
  );
  const fd = await fileHunks(dir, base, "holistic", "dup.txt", 0);
  expect(fd.hunks).toHaveLength(2);
  const ids = fd.hunks.map((h) => h.id);
  expect(new Set(ids).size).toBe(2); // unique despite identical bodies
  expect(ids.every((id) => /~\d+$/.test(id))).toBe(true);
  // Framing still holds with the suffixed ids, and each is independently ownable.
  const raw = await baseContent(dir, base, "dup.txt");
  const all = new Set(ids);
  expect(reconstructFile(raw, fd.hunks, all)).toBe(holisticContent(dir, "dup.txt"));
  expect(reconstructFile(raw, fd.hunks, new Set([ids[0]!]))).toBe(
    lines("Y", "a", "b", "c", "d", "e", "f", "X"),
  );
});

test("--unified 0 splits edits that 3 lines of context coalesce", async () => {
  // Two edits two lines apart: one hunk at U3, two at U0.
  const { dir, base } = scenario(
    { "c.txt": lines("a", "b", "c", "d", "e") },
    { "c.txt": lines("A", "b", "c", "d", "E") },
  );
  const u3 = await fileHunks(dir, base, "holistic", "c.txt", 3);
  expect(u3.hunks).toHaveLength(1);
  const u0 = await fileHunks(dir, base, "holistic", "c.txt", 0);
  expect(u0.hunks).toHaveLength(2);
  // Framing holds at U0 too, and the two edits are now independently ownable.
  const raw = await baseContent(dir, base, "c.txt");
  expect(reconstructFile(raw, u0.hunks, new Set([u0.hunks[0]!.id]))).toBe(
    lines("A", "b", "c", "d", "e"),
  );
});

test("content-hashed ids are stable across line-number shifts", async () => {
  // The same change with the same local context (≥3 identical lines each side,
  // so the U3 hunk body is byte-identical), positioned at different depths,
  // hashes to the same id — the property line ranges can't give. The leading
  // pad lines shift line numbers but stay outside the hunk.
  const common = (t: string) =>
    lines("c1", "c2", "c3", t, "c4", "c5", "c6");
  const baseChange = common("target");
  const change = common("TARGET");
  const shallow = scenario({ "s.txt": baseChange }, { "s.txt": change });
  const pad = lines("pad", "pad", "pad", "pad", "pad");
  const deep = scenario(
    { "s.txt": pad + baseChange },
    { "s.txt": pad + change },
  );
  const a = await fileHunks(shallow.dir, shallow.base, "holistic", "s.txt");
  const b = await fileHunks(deep.dir, deep.base, "holistic", "s.txt");
  expect(a.hunks).toHaveLength(1);
  expect(b.hunks).toHaveLength(1);
  expect(a.hunks[0]!.id).toBe(b.hunks[0]!.id);
});

test("middle-subset of a 3-hunk file: only the middle region flips", async () => {
  // The cursor-accounting bug class only shows when an UNOWNED hunk sits
  // between two others. Three separated single-line edits at U0 give three
  // independent hunks; own only the middle.
  const { dir, base } = scenario(
    { "t.txt": lines("A", "1", "2", "3", "B", "4", "5", "6", "C") },
    { "t.txt": lines("A1", "1", "2", "3", "B1", "4", "5", "6", "C1") },
  );
  const fd = await fileHunks(dir, base, "holistic", "t.txt", 0);
  expect(fd.hunks).toHaveLength(3);
  const raw = await baseContent(dir, base, "t.txt");
  const mid = reconstructFile(raw, fd.hunks, new Set([fd.hunks[1]!.id]));
  expect(mid).toBe(lines("A", "1", "2", "3", "B1", "4", "5", "6", "C"));
  // Owning first + last but not middle: ends flip, middle stays.
  const ends = reconstructFile(raw, fd.hunks, new Set([fd.hunks[0]!.id, fd.hunks[2]!.id]));
  expect(ends).toBe(lines("A1", "1", "2", "3", "B", "4", "5", "6", "C1"));
});

test("append at EOF with no trailing newline (oldCount 0 + newEndsNoEol)", async () => {
  // The riskiest cell: a pure insertion that reaches EOF and ends without a
  // newline. Distinct from the replacement-at-EOF no-newline case above.
  const { dir, base } = scenario(
    { "e.txt": lines("a", "b", "c") },
    { "e.txt": "a\nb\nc\nEND" }, // appended line, no final newline
  );
  const fd = await checkFraming(dir, base, "e.txt");
  const raw = await baseContent(dir, base, "e.txt");
  const all = new Set(fd.hunks.map((h) => h.id));
  expect(reconstructFile(raw, fd.hunks, all)).toBe("a\nb\nc\nEND");
  // Unowned: the base (with its trailing newline) is reproduced untouched.
  expect(reconstructFile(raw, fd.hunks, new Set())).toBe(lines("a", "b", "c"));
});

test("file deleted on the holistic side: all-owned reconstructs to empty", async () => {
  const { dir, base } = scenario(
    { "gone.txt": lines("x", "y", "z") },
    { "gone.txt": null }, // deleted on holistic
  );
  const fd = await fileHunks(dir, base, "holistic", "gone.txt");
  const raw = await baseContent(dir, base, "gone.txt");
  const all = new Set(fd.hunks.map((h) => h.id));
  expect(reconstructFile(raw, fd.hunks, all)).toBe("");
  expect(reconstructFile(raw, fd.hunks, new Set())).toBe(lines("x", "y", "z"));
});

test("line counts: per-hunk and owned-subset totals", async () => {
  const { dir, base } = scenario(
    { "m.txt": lines("keep", "old1", "old2") },
    { "m.txt": lines("keep", "new1", "new2", "new3") },
  );
  const fd = await fileHunks(dir, base, "holistic", "m.txt");
  const counts = hunkLineCounts(fd.hunks[0]!);
  expect(counts.added).toBe(3);
  expect(counts.removed).toBe(2);
  const all = new Set(fd.hunks.map((h) => h.id));
  expect(countHunkLines(fd.hunks, all)).toBe(5);
  expect(countHunkLines(fd.hunks, new Set())).toBe(0);
});
