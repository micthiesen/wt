import { git } from "../../../core/git.ts";
import type { RebaseResult } from "../../../core/stack-ops.ts";
import { findStackIdByBranch } from "../../../core/wtstate.ts";
import { blue, bold, cyan, dim, green, magenta, red, yellow } from "../../colors.ts";

export function logLine(line: string): void {
  console.log(dim(line));
}

/**
 * Resolve a stackId from the current worktree's branch, for subcommands
 * run from inside a slice without an explicit id. Returns null on a
 * detached HEAD or a branch that belongs to no manifest.
 */
export async function stackIdFromCwd(): Promise<string | null> {
  let branch = "";
  try {
    branch = (await git(["rev-parse", "--abbrev-ref", "HEAD"], process.cwd())).trim();
  } catch {
    return null;
  }
  if (!branch || branch === "HEAD") return null;
  return findStackIdByBranch(branch);
}

/**
 * Parse the shared `[stackId] [--onto <ref>]` form for rebase/replay/
 * reconcile, resolving the id from the current branch when omitted. Returns
 * the parsed target, or a numeric exit code on a usage error.
 */
export async function parseStackTarget(
  argv: string[],
  verb: string,
): Promise<{ stackId: string; onto?: string } | number> {
  let stackId: string | undefined;
  let onto: string | undefined;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === "--onto") {
      onto = argv[++i];
      if (!onto) {
        console.error(red("--onto requires a ref"));
        return 2;
      }
    } else if (a.startsWith("--")) {
      console.error(red(`unknown flag: ${a}`));
      return 2;
    } else if (!stackId) stackId = a;
    else {
      console.error(red(`unexpected arg: ${a}`));
      return 2;
    }
  }
  if (!stackId) {
    // No explicit id: resolve from the current worktree's branch so
    // `/restack` (and a human in a slice) can just run the command bare.
    stackId = (await stackIdFromCwd()) ?? undefined;
    if (stackId) console.log(dim(`stack ${stackId} (resolved from current branch)`));
  }
  if (!stackId) {
    console.error(
      red(
        `usage: wt stack ${verb} [<stackId>] [--onto <ref>]\n` +
          "  (no stackId given and the current branch isn't a tracked slice)",
      ),
    );
    return 2;
  }
  return onto ? { stackId, onto } : { stackId };
}

/** Render a rebase/replay result; returns the process exit code. */
export function reportReplayResult(
  stackId: string,
  verb: string,
  result: RebaseResult,
): number {
  if (!result.ok) {
    console.error(red(result.error));
    if (result.conflict) {
      // Hand off to the resolving skill — wt never auto-resolves conflicts.
      if (result.failedBranch) console.error(yellow(`  failing branch: ${result.failedBranch}`));
      if (result.backupBranch) console.error(yellow(`  backup branch:  ${result.backupBranch}`));
      console.error(dim("  resolve in that worktree, then re-run `wt stack replay` (or /restack)."));
      return 3;
    }
    return 1;
  }
  if (result.output) console.log(dim(result.output));
  console.log(green(`✓ ${verb} ${bold(stackId)}`));
  return 0;
}

/**
 * Connector color for a forked lane (mirrors `laneColor` in the TUI theme,
 * with ansi instead of hex). Lane 0 — the main spine and every linear
 * stack — stays dim; forked siblings each pick a distinct hue. Avoids
 * green/red so a lane tint never reads as a status/drift marker.
 */
export const CLI_LANE_PALETTE = [magenta, cyan, blue, yellow] as const;
export function laneColor(lane: number, glyph: string): string {
  if (lane <= 0) return dim(glyph);
  return CLI_LANE_PALETTE[(lane - 1) % CLI_LANE_PALETTE.length]!(glyph);
}
