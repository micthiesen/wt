/**
 * Graceful-degradation reducer for inline row segments.
 *
 * Each row in the details pane is conceptually a list of `·`-separated
 * segments (status verb, diff stats, ages, sync counts, ...). When the
 * pane is narrow opentui's native truncate clips the trailing segments
 * indiscriminately — so the most-load-bearing piece (sync counts) can
 * disappear while the least-interesting one (an "X created 4d ago"
 * note) survives just because it sat earlier in the chain.
 *
 * This module flips that. Authors declare each segment with:
 *   - a `tier`: lower number = keep longer; ties broken by current width
 *   - an ordered list of `modes`: full → tighter → ... → drop sentinel
 *
 * `fitSegments` then runs the same shape of greedy reducer as
 * `core/diff/fit.ts`: while the rendered total exceeds `budget`, pick
 * the worst contributor (highest tier, largest current width within the
 * tier) and step it down one mode. Stops once it fits or every segment
 * is dropped. The dropped sentinel is just `width: 0`, `render: () =>
 * null` — no special-casing in the reducer.
 *
 * The output is joined with ` · ` between surviving segments. Callers
 * still wrap the result in `<text wrapMode="none" truncate>` so the
 * native binary clips the rest if even the all-dropped result busts
 * the budget (very narrow pane, single segment that can't compact).
 */
import { Fragment, type ReactNode } from "react";

export type SegmentMode = {
  /** Visible cell width — sum of `Bun.stringWidth` for plain text + literal counts for icon-cells. */
  width: number;
  render: () => ReactNode;
};

export type Segment = {
  /** Stable React key + log/debug label. */
  key: string;
  /**
   * Drop priority: lower = kept later. The reducer steps the segment
   * with the *highest* tier first; ties broken by largest current width.
   */
  tier: number;
  /**
   * Ordered most-verbose → most-compact. The last entry is the drop
   * sentinel (width 0, render → null) so authors don't reinvent it
   * — `dropMode()` exists for convenience.
   */
  modes: SegmentMode[];
};

/** Convenience: the standard "this segment is gone" mode. */
export const dropMode: SegmentMode = { width: 0, render: () => null };

export type FitResult = {
  /** Joined ReactNode ready to drop into a `<text>`. */
  rendered: ReactNode;
  /** Per-segment chosen mode index (`modes.length - 1` = dropped). */
  modeIndex: number[];
  /** Sum of surviving widths (excludes separators). */
  contentWidth: number;
};

/**
 * Reduce `segments` to fit `budget` cells. The separator joining
 * surviving segments takes `sepWidth` cells each (default 3 for ` · `).
 * Returns the joined ReactNode plus per-segment chosen mode for
 * memoization / telemetry.
 *
 * Mutation: this function does *not* mutate `segments`. Authors can
 * reuse the same array across renders without `slice()`-ing.
 */
export function fitSegments(
  segments: readonly Segment[],
  budget: number,
  sepWidth = 3,
): FitResult {
  if (segments.length === 0) {
    return { rendered: null, modeIndex: [], contentWidth: 0 };
  }
  // Per-segment current mode index. Start everyone at full (mode 0).
  const idx = new Array<number>(segments.length).fill(0);

  function widthAt(i: number): number {
    return segments[i]!.modes[idx[i]!]!.width;
  }
  function totalCells(): number {
    let content = 0;
    let alive = 0;
    for (let i = 0; i < segments.length; i++) {
      const w = widthAt(i);
      if (w > 0) {
        content += w;
        alive++;
      }
    }
    return alive > 1 ? content + (alive - 1) * sepWidth : content;
  }

  // Each iteration strictly decreases either some idx[i] (bounded by
  // modes.length - 1) or the total width — guarantees termination.
  while (totalCells() > budget) {
    // Worst contributor: highest tier among segments that aren't yet
    // at their floor mode. Ties broken by largest current width.
    let target = -1;
    let targetTier = -Infinity;
    let targetWidth = -1;
    for (let i = 0; i < segments.length; i++) {
      const seg = segments[i]!;
      if (idx[i]! >= seg.modes.length - 1) continue; // already at floor
      const tier = seg.tier;
      const w = widthAt(i);
      if (
        tier > targetTier ||
        (tier === targetTier && w > targetWidth)
      ) {
        target = i;
        targetTier = tier;
        targetWidth = w;
      }
    }
    if (target === -1) break; // Nothing left to compact.
    idx[target]!++;
  }

  // Render survivors with separators. Skip width-0 (dropped) segments —
  // including those that hit the drop sentinel mid-reduce. Each survivor
  // wraps in a keyed Fragment that includes its leading separator (when
  // not first) so React doesn't see unkeyed siblings in the array.
  const out: ReactNode[] = [];
  let contentWidth = 0;
  let first = true;
  for (let i = 0; i < segments.length; i++) {
    const seg = segments[i]!;
    const mode = seg.modes[idx[i]!]!;
    if (mode.width === 0) continue;
    contentWidth += mode.width;
    out.push(
      <Fragment key={seg.key}>
        {first ? null : " · "}
        {mode.render()}
      </Fragment>,
    );
    first = false;
  }
  return { rendered: out, modeIndex: idx, contentWidth };
}
