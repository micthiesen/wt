/**
 * Strict, centralised gate for everything that asserts ownership of
 * a worktree's SST stage — the deploy badge, the `sst remove` step,
 * the doctor check, the URL we hand out.
 *
 * The trap this exists to prevent: a user runs
 * `scripts/deployProductionApp.sh <prod-stage>` (or any other
 * `pnpm sst deploy --stage <foreign>`) from inside the worktree.
 * SST happily writes `.sst/outputs.json` with the foreign deploy's
 * resources, while `.sst/stage` stays pinned to our dev stage.
 * Naively, "outputs.json non-empty" looks deployed — and `wt rm`
 * would offer to `sst remove` based on that, even though the
 * outputs belong to production. We must NEVER act on that signal.
 *
 * Rules enforced here:
 *   1. The "stage we're allowed to manage" is purely a function of
 *      the slug (`expectedStage`). Never trusted from disk.
 *   2. To take any destructive action, `.sst/stage` must exist and
 *      match the expected name exactly (`safeStage`).
 *   3. To call a worktree "deployed", the outputs must reference our
 *      expected stage. Outputs that mention only foreign stages are
 *      treated identically to "never deployed" — no badge, no
 *      auto-destroy, nothing.
 */
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

import { config } from "./config.ts";
import { computeStage } from "./stage.ts";
import type { Worktree } from "./types.ts";

type WtRef = Pick<Worktree, "slug" | "path">;

/**
 * Deterministic stage name for a worktree. Computed from the slug;
 * never read from on-disk state. This is the ONLY stage value
 * destructive code paths are permitted to act on — see
 * `lifecycle.removeWorktree`, which passes this verbatim as
 * `--stage` to `sst remove`.
 */
export function expectedStage(wt: Pick<Worktree, "slug">): string {
  return computeStage(wt.slug);
}

export type StageSafety =
  | { ok: true; stage: string }
  | { ok: false; reason: string };

/**
 * Gate every destructive stage operation through this. Refuses when
 * the expected name lacks our configured prefix, when no `.sst/stage`
 * is pinned, or when the pin disagrees with the expected name.
 */
export function safeStage(wt: WtRef): StageSafety {
  const expected = expectedStage(wt);
  const prefix = config.stage.prefix;
  if (!prefix || !expected.startsWith(prefix)) {
    return {
      ok: false,
      reason: `expected stage "${expected}" lacks configured prefix "${prefix}"`,
    };
  }
  const pinned = readPinnedStage(wt.path);
  if (pinned === null) return { ok: false, reason: "no .sst/stage pinned" };
  if (pinned !== expected) {
    return {
      ok: false,
      reason: `.sst/stage is "${pinned}", expected "${expected}" — refusing to manage a foreign stage`,
    };
  }
  return { ok: true, stage: expected };
}

/**
 * The single boolean every caller should consult before treating a
 * worktree as "deployed" for destructive or ownership-asserting
 * purposes. True iff:
 *
 *   - `safeStage` is ok (pinned matches expected, prefix correct).
 *   - `.sst/outputs.json` exists, parses, and references the expected
 *     stage somewhere in its values.
 *
 * Anything else — missing/empty/foreign outputs — looks identical
 * to "never deployed" to the rest of the app.
 */
export function isOurStageDeployed(wt: WtRef): boolean {
  if (!safeStage(wt).ok) return false;
  return outputsReferenceStage(wt.path, expectedStage(wt));
}

function readPinnedStage(wtPath: string): string | null {
  const file = join(wtPath, ".sst", "stage");
  if (!existsSync(file)) return null;
  try {
    return readFileSync(file, "utf8").trim() || null;
  } catch {
    return null;
  }
}

/**
 * Cheap substring check against the raw JSON. Output values like
 * `${stage}-app-<resource>-<hash>` mean the expected stage name
 * appears verbatim in the file when it's ours. Stage names are
 * `[a-z0-9-]+`, so JSON string escaping is a no-op and we don't
 * need to parse to test membership.
 */
function outputsReferenceStage(wtPath: string, stage: string): boolean {
  const file = join(wtPath, ".sst", "outputs.json");
  if (!existsSync(file)) return false;
  try {
    return readFileSync(file, "utf8").includes(stage);
  } catch {
    return false;
  }
}
