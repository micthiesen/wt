/**
 * Greedy graceful-degradation reducer.
 *
 * Treats the diff as a list of `Part`s, each at some `FileMode`.
 * Repeatedly picks the worst contributor — lowest priority tier
 * first, largest current size within the tier — and steps it down
 * one mode. Stops once total size fits the budget or every part is
 * `dropped`.
 *
 * The priority lattice means a tiny `package.json` change can sit at
 * `full` for the whole reduction while a giant `pnpm-lock`-adjacent
 * config gets crushed first. Source files are degraded last, in
 * descending size order, and only after all lower-tier files have
 * been exhausted at their floor.
 */
import { MODE_ORDER, type FileMode, type Part } from "./parts.ts";
import { renderPart } from "./render.ts";

export type ModeCounts = Record<FileMode, number>;

export type FitResult = {
  /** Concatenated rendered output, files in their original diff order. */
  rendered: string;
  /** How many files ended up at each mode; for activity-pane logging. */
  counts: ModeCounts;
};

/**
 * Priority tiers — lower number = degrade later. Source code is the
 * most informative for the LLM, so it sits at tier 1. Tests / docs /
 * schemas are tier 2. Configs / lockfile-adjacent / CI are tier 3
 * and get sacrificed first.
 *
 * Fall-through default is tier 2 (medium) so unknown extensions
 * don't get prioritised over real source code, but also aren't first
 * on the chopping block.
 */
function priority(path: string): number {
  // Test files first — `.test.ts` would otherwise match the source
  // tier below. Stories files (Storybook etc.) similarly.
  if (/\.(test|spec|stories|e2e)\.(ts|tsx|js|jsx|mjs|cjs|py|rb|go)$/.test(path)) {
    return 2;
  }
  // CI / build / config directories — tier 3 wholesale.
  if (
    path.startsWith(".github/") ||
    path.startsWith(".circleci/") ||
    path.startsWith(".gitlab/") ||
    path.startsWith(".husky/") ||
    path.startsWith(".vscode/") ||
    path.startsWith(".idea/")
  ) {
    return 3;
  }
  const lower = path.toLowerCase();
  // Config-y file extensions — tier 3.
  if (/\.(json|ya?ml|toml|env|conf|cfg|ini|lock|properties)$/.test(lower)) {
    return 3;
  }
  // Schema / data-shape files — tier 2.
  if (/\.(sql|graphql|gql|prisma|proto|xsd|wsdl)$/.test(lower)) {
    return 2;
  }
  // Docs — tier 2.
  if (/\.(md|mdx|rst|txt|adoc)$/.test(lower)) {
    return 2;
  }
  // Source code — tier 1.
  if (
    /\.(ts|tsx|js|jsx|mjs|cjs|py|go|rs|java|kt|kts|swift|c|cc|cpp|cxx|h|hh|hpp|hxx|rb|php|cs|fs|fsx|scala|elm|ex|exs|erl|clj|cljs|m|mm|vue|svelte|astro|sh|bash|zsh|fish|lua|dart|nim|zig|hs|ml|mli|r|jl)$/.test(
      lower,
    )
  ) {
    return 1;
  }
  return 2;
}

/**
 * Reduce `parts` to fit `budgetChars`. Mutates the parts' `mode`
 * field as it goes; callers that need the originals should clone
 * upstream. Termination is guaranteed: each iteration strictly
 * decreases the total mode-rank or drops a part, both bounded.
 */
export function fitParts(parts: Part[], budgetChars: number): FitResult {
  // Render cache keyed by (path, mode). Mode changes invalidate
  // automatically because the key includes mode.
  const cache = new Map<string, string>();
  function rendered(part: Part): string {
    const key = `${part.path}\x1f${part.mode}`;
    let s = cache.get(key);
    if (s === undefined) {
      s = renderPart(part, part.mode);
      cache.set(key, s);
    }
    return s;
  }
  function totalChars(): number {
    let n = 0;
    for (const p of parts) n += rendered(p).length;
    return n;
  }

  while (totalChars() > budgetChars) {
    // Worst contributor: max priority tier (= least important),
    // largest current rendered size within the tier.
    let target: Part | null = null;
    let targetTier = -1;
    let targetSize = -1;
    for (const p of parts) {
      if (p.mode === "dropped") continue;
      const tier = priority(p.path);
      const size = rendered(p).length;
      if (
        tier > targetTier ||
        (tier === targetTier && size > targetSize)
      ) {
        target = p;
        targetTier = tier;
        targetSize = size;
      }
    }
    if (!target) break; // Everything dropped; nothing more to give.
    const nextIdx = MODE_ORDER.indexOf(target.mode) + 1;
    target.mode = MODE_ORDER[nextIdx]!;
  }

  const counts: ModeCounts = { full: 0, tight: 0, hunks: 0, dropped: 0 };
  const out: string[] = [];
  for (const p of parts) {
    counts[p.mode]++;
    const text = rendered(p);
    if (text) out.push(text);
  }
  return { rendered: out.join(""), counts };
}

/**
 * One-line human-readable summary of mode counts, for the activity
 * pane. Returns null when nothing was compacted (every file at full)
 * so the log line stays clean in the common case.
 */
export function formatCompaction(counts: ModeCounts): string | null {
  const total = counts.full + counts.tight + counts.hunks + counts.dropped;
  if (total === 0) return null;
  if (counts.full === total) return null;
  const parts: string[] = [];
  if (counts.full > 0) parts.push(`${counts.full} full`);
  if (counts.tight > 0) parts.push(`${counts.tight} tight`);
  if (counts.hunks > 0) parts.push(`${counts.hunks} hunks-only`);
  if (counts.dropped > 0) parts.push(`${counts.dropped} dropped`);
  return parts.join(", ");
}
