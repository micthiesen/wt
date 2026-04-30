/**
 * Build the prompt context for AI worktree summaries.
 *
 * Pipeline:
 *   1. Run `git diff --stat` and `git log --reverse --format=%s` for
 *      the always-included header (these are tiny and high-signal).
 *   2. Run `git diff -U3 -W --diff-algorithm=patience` once for the
 *      detailed body, with static excludes applied via pathspec.
 *   3. Parse into per-file `Part`s, hand them to `fitParts` for
 *      graceful degradation under the token budget.
 *   4. Hash the *unfiltered* diff so the cache key is content-stable
 *      across rebases and amendments — equivalent diffs share a
 *      summary regardless of HEAD SHA.
 */
import { CryptoHasher } from "bun";

import { config } from "../config.ts";
import { run } from "../proc.ts";

import { fitParts, formatCompaction, type ModeCounts } from "./fit.ts";
import { parseDiff } from "./parts.ts";

/**
 * Pathspec excludes applied to every git invocation. Things in here
 * are never worth showing the LLM — lockfiles, snapshots, generated
 * code, build artifacts. Mirrors the list in
 * `~/.dotfiles/git/.config/git/hooks/prepare-commit-msg`.
 */
const STATIC_EXCLUDES = [
  ":!package-lock.json",
  ":!yarn.lock",
  ":!pnpm-lock.yaml",
  ":!bun.lock",
  ":!bun.lockb",
  ":!Cargo.lock",
  ":!*.min.js",
  ":!*.min.css",
  ":!*.map",
  ":!*.snap",
  ":!dist/*",
  ":!build/*",
  ":!*.generated.*",
  ":!*.g.dart",
  ":!*_generated.go",
  ":!*.pb.go",
  ":!*.pb.ts",
  ":!*.d.ts",
  ":!migrations/*.sql",
] as const;

/**
 * Conservative chars-per-token estimate for Gemma-style tokenizers.
 * Used only to size-budget the prompt; the model itself does the
 * authoritative tokenisation.
 */
const CHARS_PER_TOKEN = 3.5;

/**
 * Char overhead reserved for the system prompt + scaffolding ("File
 * summary:", "Detailed changes:", etc.) before files compete for
 * what's left.
 */
const SCAFFOLD_OVERHEAD_CHARS = 500;

export type DiffContext = {
  /** SHA-256 prefix of the unfiltered diff; the AI summary cache key. */
  hash: string;
  /** Final assembled prompt body sent to the model. */
  prompt: string;
  /** Per-mode file counts after compaction. */
  counts: ModeCounts;
  /** Total file count before any dropping. */
  filesTotal: number;
};

export async function buildDiffContext(wtPath: string): Promise<DiffContext | null> {
  if (!config.ai) return null;
  const base = `origin/${config.branch.base}`;

  const stat = await diffStat(wtPath, base);
  // No committed changes against base means there's nothing for the
  // LLM to summarise — bail before doing any more work.
  if (!stat) return null;

  const log = await commitLog(wtPath, base);
  const rawDiff = await fullDiff(wtPath, base);

  // Hash the unfiltered diff so summaries cache by content. Filter
  // tweaks (mode changes, exclude list updates) don't invalidate the
  // cache, which is what we want — the dropped pieces don't affect
  // the summary's quality enough to justify a regen.
  const hash = shortHash(`${base}\n${stat}\n${rawDiff}`);

  const parts = parseDiff(rawDiff);
  const filesTotal = parts.length;

  const headerChars = stat.length + log.length + SCAFFOLD_OVERHEAD_CHARS;
  const totalBudgetChars = config.ai.maxInputTokens * CHARS_PER_TOKEN;
  const fileBudget = Math.max(1000, totalBudgetChars - headerChars);
  const fit = fitParts(parts, fileBudget);

  const sections: string[] = [];
  sections.push(`File summary:\n${stat}`);
  if (log) sections.push(`Commit messages (oldest first):\n${log}`);
  if (fit.rendered) sections.push(`Detailed changes:\n${fit.rendered}`);
  const note = formatCompaction(fit.counts);
  if (note) sections.push(`(compaction: ${note})`);

  const prompt = sections.join("\n\n");
  // Defensive: if the assembled prompt still busts the budget (huge
  // commit log, monstrous stat), hard-truncate the tail. Should be
  // very rare in practice.
  const finalPrompt =
    prompt.length > totalBudgetChars
      ? `${prompt.slice(0, Math.floor(totalBudgetChars))}\n\n(prompt truncated)`
      : prompt;

  return {
    hash,
    prompt: finalPrompt,
    counts: fit.counts,
    filesTotal,
  };
}

// Three-dot (`base...HEAD` = `merge-base(base, HEAD)..HEAD`) so the diff
// reflects only what this branch contributes. Two-dot would show the
// tree delta between base and HEAD, which on a branch that's purely
// behind base produces the *inverse* of the commits base has gained —
// the LLM then summarises those as if they were this branch's work.
async function diffStat(wtPath: string, base: string): Promise<string> {
  const r = await run(
    ["git", "diff", "--stat", `${base}...HEAD`, "--", ...STATIC_EXCLUDES],
    { cwd: wtPath, timeoutMs: 10_000 },
  );
  return r.exitCode === 0 ? r.stdout.trim() : "";
}

async function commitLog(wtPath: string, base: string): Promise<string> {
  const r = await run(
    ["git", "log", "--reverse", "--format=%s", `${base}..HEAD`],
    { cwd: wtPath, timeoutMs: 5_000 },
  );
  return r.exitCode === 0 ? r.stdout.trim() : "";
}

async function fullDiff(wtPath: string, base: string): Promise<string> {
  const r = await run(
    [
      "git",
      "diff",
      "-U3",
      "-W",
      "--diff-algorithm=patience",
      "--ignore-space-change",
      "--ignore-blank-lines",
      `${base}...HEAD`,
      "--",
      ...STATIC_EXCLUDES,
    ],
    { cwd: wtPath, timeoutMs: 15_000 },
  );
  return r.exitCode === 0 ? r.stdout : "";
}

function shortHash(s: string): string {
  return new CryptoHasher("sha256").update(s).digest("hex").slice(0, 16);
}
