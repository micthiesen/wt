/**
 * Centralised gate for everything that asserts ownership of a
 * worktree's SST stage — the deploy badge, the `sst remove` step,
 * the doctor check, the URL we hand out.
 *
 * `.sst/stage` is the source of truth for which stage a worktree
 * owns: it's what `wt new` pins, what SST itself deploys under, and
 * what you can hand-edit to point a worktree at an existing stage
 * (e.g. a renamed worktree, or one re-pinned to a deploy whose hash
 * no longer matches the slug). We read it directly rather than
 * re-deriving from the slug, so the badge and `sst remove` follow
 * what's actually deployed.
 *
 * The safety net is the PREFIX, not the exact hash. Every personal
 * stage lives under `config.stage.prefix` (e.g. `michael-`); a
 * production stage does not. We only treat a worktree as deployed, or
 * hand a `--stage` to `sst remove`, when the pinned stage carries that
 * prefix — so you can never act outside your own namespace.
 *
 * The trap this still prevents: a `deployProductionApp.sh <prod-stage>`
 * (or any `pnpm sst deploy --stage <foreign>`) run from inside the
 * worktree fills `.sst/outputs.json` with production resources while
 * `.sst/stage` stays pinned to your personal stage. Outputs that don't
 * reference the pinned stage read as "not deployed", and a pin pointing
 * at a non-prefixed (foreign/prod) stage is refused outright.
 *
 * Rules enforced here:
 *   1. The stage a worktree owns is the pinned `.sst/stage` when it
 *      carries the configured prefix; otherwise the slug-derived
 *      `computeStage` (uninitialised worktrees) — see `expectedStage`.
 *   2. Destructive actions require `safeStage.ok`: a `.sst/stage` pin
 *      that exists and carries the prefix.
 *   3. "Deployed" additionally requires `.sst/outputs.json` to
 *      reference that exact stage.
 */
import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

import { config } from "./config.ts";
import { computeStage } from "./stage.ts";
import type { Worktree } from "./types.ts";

type WtRef = Pick<Worktree, "slug" | "path">;

/**
 * The stage a worktree owns: the pinned `.sst/stage` when present and
 * carrying the configured prefix, else the slug-derived default for an
 * uninitialised worktree. Used for the deploy URL, the `{{stage}}`
 * action variable, and display. Destructive paths go through
 * `safeStage`, which refuses the slug fallback (no pin → nothing to
 * manage).
 */
export function expectedStage(wt: WtRef): string {
  const pinned = readPinnedStage(wt.path);
  const prefix = config.stage.prefix;
  if (pinned !== null && prefix && pinned.startsWith(prefix)) return pinned;
  return computeStage(wt.slug);
}

export type StageSafety =
  | { ok: true; stage: string }
  | { ok: false; reason: string };

/**
 * Gate every destructive stage operation through this. The returned
 * `stage` is the pinned `.sst/stage` verbatim — what's actually
 * deployed — and is what `sst remove --stage` receives. Refuses when
 * nothing is pinned, when no prefix is configured, or when the pin
 * lacks the personal prefix (a foreign/production stage we must never
 * touch).
 */
export function safeStage(wt: WtRef): StageSafety {
  const prefix = config.stage.prefix;
  const pinned = readPinnedStage(wt.path);
  if (pinned === null) return { ok: false, reason: "no .sst/stage pinned" };
  if (!prefix || !pinned.startsWith(prefix)) {
    return {
      ok: false,
      reason: `.sst/stage is "${pinned}", which lacks the personal prefix "${prefix}" — refusing to manage a foreign stage`,
    };
  }
  return { ok: true, stage: pinned };
}

/**
 * The single boolean every caller should consult before treating a
 * worktree as "deployed" for destructive or ownership-asserting
 * purposes. True iff:
 *
 *   - `safeStage` is ok (pinned exists and carries the prefix).
 *   - `.sst/outputs.json` exists, parses, and references that pinned
 *     stage somewhere in its values.
 *
 * Anything else — missing/empty/foreign outputs — looks identical
 * to "never deployed" to the rest of the app.
 */
export function isOurStageDeployed(wt: WtRef): boolean {
  const safe = safeStage(wt);
  if (!safe.ok) return false;
  return outputsReferenceStage(wt.path, safe.stage);
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
