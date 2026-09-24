/**
 * The LEFT-hand glyphs of a worktree row: the stack rail and the
 * status marker. Twin of `badge-cluster.tsx`, which owns the right-hand
 * side — between them, a row rendered in the list pane and the same row
 * rendered in the detail pane's section summary are built from one set
 * of components and cannot drift.
 *
 * They drifted before this module existed: the list moved its leftmost
 * slot to the work-status dot and the section summary kept rendering
 * the old git status badge, so folding a section silently changed what
 * every row's first glyph meant.
 */
import { isWorkStatusStale, owesPostMergeVerification } from "../core/work-status.ts";
import { rowHasLanded } from "./app-helpers.ts";
import {
  STACK_CONNECTOR,
  spineLayout,
  type SpineCell,
} from "../core/stack-layout.ts";
import { StatusKind } from "../core/types.ts";
import type { DerivedState } from "../core/harness/status.ts";
import { config } from "../core/config.ts";
import { statusBadge, workStatusBadge, type Badge } from "./badges.ts";
import { NF } from "./icons.ts";
import type { Landing } from "./landing.ts";
import type { WorktreeRow } from "./hooks/useWorktreeRows.ts";
import { laneColor, theme } from "./theme.ts";

/** Cells the marker occupies including its trailing gap. */
export const STATUS_MARKER_CELLS = 3;

/**
 * Whether the git-derived status keeps the marker slot: the rare, loud
 * states (busy op / path missing / branch gone / merged) still render
 * their glyph there. `dirty` moved to the badge cluster and `clean`
 * renders nothing — the slot's steady-state occupant is the
 * work-status dot (`workStatusBadge`).
 */
function statusKeepsMarker(kind: StatusKind): boolean {
  return kind !== StatusKind.Dirty && kind !== StatusKind.Clean;
}

/** Release position changes the shape only; `u` keeps ownership of the hue. */
export function releaseMarkerBadge(landing: Landing, workBadge: Badge): Badge {
  if (landing === "production") return { ...workBadge, glyph: NF.production };
  if (landing === "base") return { ...workBadge, glyph: NF.staging };
  return workBadge;
}

/** Stale signal for the row's work-status dot (see `isWorkStatusStale`). */
export function rowWorkStale(row: WorktreeRow): boolean {
  return isWorkStatusStale(row.work, row.fields.gitActivity.data?.lastCommitMs ?? null);
}

/**
 * Leftmost glyph — the loud git states (busy / missing / gone /
 * merged) when present, else the work-status dot (a dim hollow
 * default when nothing is asserted, so the column never has holes).
 * Background refetch state is hinted via the spinner badge in the
 * right cluster instead, so it doesn't masquerade as a primary
 * status. Archived rows render dim.
 */
export function StatusMarker({
  row,
  sessionState,
}: {
  row: WorktreeRow;
  sessionState: DerivedState | undefined;
}) {
  // With a promotion branch configured, release position owns the shape
  // and work status owns the color. Without it, preserve the legacy loud
  // git marker unless post-merge verification is owed.
  const landed = rowHasLanded(row);
  const owes = owesPostMergeVerification(row.work, landed);
  const workBadge = workStatusBadge(row.work, sessionState, rowWorkStale(row), landed);
  const base = row.landedOn && config.branch.production
    ? releaseMarkerBadge(row.landedOn, workBadge)
    : !owes && statusKeepsMarker(row.status.kind)
      ? statusBadge(row.status)
      : workBadge;
  const fg = row.archived ? theme.fgDim : base.fg;
  return (
    <box flexShrink={0} flexDirection="row">
      {/* Mirror the right-cluster pattern: width=2 box for the icon,
          then a width=1 box for the gap. Same shape that produces
          tight left-aligned icons over there. Every row gets this,
          stacked or not, so the dot column never has holes. */}
      <box width={2} flexShrink={0}>
        <text fg={fg}>{base.glyph}</text>
      </box>
      <box width={1} flexShrink={0}>
        <text> </text>
      </box>
    </box>
  );
}

/**
 * Stack connector gutter — a structural rail drawn to the LEFT of the
 * status marker, never in place of it.
 *
 * It used to REPLACE the dot with a connector + `01`/`02` ordinal,
 * which cost stacked rows the one glyph the board is scanned by: two
 * worktrees could both be ready/high and blocked on a human and render
 * with no status indicator at all, purely because they were stack
 * parents. Stacked work was invisible to the primary scanning
 * behaviour.
 *
 * The ordinals are gone too, and that's a correctness fix rather than a
 * preference: `01/02/03` asserts a linear chain, but a fork's children
 * are SIBLINGS off one parent with no order between them. Numbering
 * them claims a merge order that doesn't exist, and a reader who trusts
 * it can sequence dependent work backwards. Depth expresses fan vs
 * chain for free and can't lie: siblings share a column, a chain steps
 * right. If ordinals are ever wanted, derive them from merge EDGES,
 * which actually encode order — stack position never did.
 *
 * `cell` comes from `spineLayout` over the rows drawn AROUND this one
 * (null → blank gutter); `cells` is the gutter width the whole surface
 * shares, so the marker column stays straight across stacked and
 * unstacked rows and the eye still runs down it.
 */
export function StackConnector({
  row,
  cell,
  cells,
}: {
  row: WorktreeRow;
  cell: SpineCell | null;
  cells: number;
}) {
  if (cells === 0) return null;
  const col = cell ? Math.min(cell.col, cells - 1) : -1;
  // One lane color for the whole rail: the continuations belong to
  // ancestors, but an ancestor's lane only differs from its
  // descendant's across a fork, where the column already changed.
  const fg = row.stack ? laneColor(row.stack.lane) : theme.fgDim;
  return (
    <box flexShrink={0} flexDirection="row">
      {Array.from({ length: cells }, (_, i) => (
        <box key={i} width={1} flexShrink={0}>
          <text fg={fg}>
            {!cell || i > col
              ? " "
              : i === col
                ? cell.glyph
                : cell.trail[i]
                  ? STACK_CONNECTOR.trail
                  : " "}
          </text>
        </box>
      ))}
    </box>
  );
}

/**
 * Width of the shared stack gutter: enough columns for the deepest
 * VISIBLE spine, capped so a pathological chain can't eat the label
 * column, and ZERO when nothing on the surface draws a rail — a board
 * with no stacks pays nothing for the feature. Taking it from the laid
 * out cells rather than from global stack depth means a stack whose
 * root is filed elsewhere doesn't reserve a column it never draws in.
 */
/**
 * Spine cells for every row of a surface, keyed by slug. Each group is
 * one CONTIGUOUS run of rows as drawn — a section in the list, the
 * member list in the section summary — because a rail may only connect
 * rows that are actually adjacent on screen (see `spineLayout`).
 * Non-stacked rows are dropped rather than passed through as gaps.
 */
export function rowSpine(
  groups: readonly (readonly WorktreeRow[])[],
): Map<string, SpineCell> {
  const out = new Map<string, SpineCell>();
  for (const group of groups) {
    const members = group
      .filter((r) => r.stack)
      .map((r) => ({
        key: r.wt.slug,
        branch: r.wt.branch,
        parentBranch: r.stackedOn?.branch ?? null,
      }));
    for (const [slug, cell] of spineLayout(members)) out.set(slug, cell);
  }
  return out;
}

export function spineGutterCells(spine: ReadonlyMap<string, SpineCell>): number {
  let max = -1;
  for (const cell of spine.values()) max = Math.max(max, cell.col);
  return Math.min(max + 1, 3);
}
