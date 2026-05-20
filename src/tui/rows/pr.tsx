import { memo, useMemo } from "react";

import type { MergeabilityEntry } from "../../core/graphite-api.ts";
import type {
  PrChecks,
  PrReview,
  PullRequest,
  RabbitStatus,
} from "../../core/types.ts";
import { pluralize } from "../../core/text.ts";
import { prStateBadge } from "../badges.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";
import { NF } from "../icons.ts";
import { theme } from "../theme.ts";
import { fitSegments, type Segment } from "./fit.tsx";
import type { RowModule } from "./types.ts";

function checksLabel(c: PrChecks): { glyph: string; text: string; fg: string } | null {
  switch (c) {
    case "pass":
      return { glyph: NF.checkPass, text: "checks", fg: theme.ok };
    case "fail":
      return { glyph: NF.checkFail, text: "checks", fg: theme.err };
    case "pending":
      return { glyph: NF.checkPend, text: "checks pending", fg: theme.warn };
    default:
      return null;
  }
}

/**
 * Review badge mapping. Approved / changes-requested get distinct shapes
 * (thumbs up/down). `pending` and `unrequested` share the eye glyph and
 * are told apart by color (orange = asked + waiting, dim = nobody asked
 * yet) — the one spot color is load-bearing here, chosen so review-
 * pending doesn't reuse the CI clock (`checkPend`) and collide with it.
 * Distinct family from CI checks otherwise so the signals don't blur.
 */
function reviewLabel(r: PrReview): { glyph: string; text: string; fg: string } | null {
  switch (r) {
    case "approved":
      return { glyph: NF.thumbsUp, text: "approved", fg: theme.ok };
    case "changes_requested":
      return { glyph: NF.thumbsDown, text: "changes requested", fg: theme.err };
    case "pending":
      return { glyph: NF.eye, text: "review pending", fg: theme.warn };
    case "unrequested":
      return { glyph: NF.eye, text: "no reviewers", fg: theme.fgDim };
    default:
      return null;
  }
}

/**
 * CodeRabbit badge mapping. Single carrot glyph, color-coded — fits
 * the existing "carrots / grazing / resting" vocab and stays visually
 * distinct from human review (thumbs/eye) and CI checks
 * (circles). Color is load-bearing here, accepted as the "if possible"
 * exception to the no-color-only rule since the paw family doesn't
 * have clean state-specific variants. Hidden on draft PRs at the
 * segment level (see `buildPrSegments`) since CR's "review skipped"
 * outcome on drafts collapses to status=COMPLETED, which the rollup
 * misreads as "resting" — gating on `isDraft` is the simpler fix and
 * mirrors review's draft-hide convention.
 */
function rabbitLabel(
  rb: RabbitStatus,
): { glyph: string; full: string; tiny: string; fg: string } | null {
  switch (rb.state) {
    // CR echoes the human-review palette but one notch softer: grazing↔
    // pending (yellow), resting↔approved (green). Unresolved threads are
    // "address these", not a rejection — so magenta (the `asking`
    // look-here tier), not changes_requested red.
    case "unresolved":
      return {
        glyph: NF.carrot,
        full: pluralize(rb.unresolved, "carrot"),
        tiny: String(rb.unresolved),
        fg: theme.info,
      };
    case "pending":
      return { glyph: NF.carrot, full: "grazing", tiny: "", fg: theme.warn };
    case "clean":
      return { glyph: NF.carrot, full: "resting", tiny: "", fg: theme.ok };
    default:
      return null;
  }
}

/**
 * Graphite mergeability label. Only the statuses that add information
 * beyond the existing PR/review/checks badges get rendered; the rest
 * (DRAFT, CHANGES_REQUESTED, NEEDS_APPROVAL(S), NEEDS_REVIEWERS) are
 * suppressed because the same signal is already on the line via
 * `prStateBadge`/`reviewLabel`. Four signals survive:
 *
 *   - `MERGEABLE`        — every gate green, ready to ship. Composite
 *                          signal, not derivable from any single badge.
 *   - `FAILING_REQUIRED` — required checks failing. Distinguishes
 *                          "blocked" from "optional check is angry"
 *                          (our `pr.checks === "fail"` doesn't).
 *   - `UNRESOLVED_COMMENTS` — human-authored unresolved review threads.
 *                          The carrot badge only counts CodeRabbit
 *                          threads, so this fills a real gap.
 *
 *   - `QUEUED` / `QUEUED_TO_MERGE` / `RUNNING` — armed in the Graphite
 *                          merge queue. `RUNNING` = required CI in
 *                          flight, `QUEUED_TO_MERGE` = waiting its turn;
 *                          we collapse all to a single `queued to merge`
 *                          in the cyan "in-flight" tier. The adjacent
 *                          checks badge already conveys CI status, so the
 *                          mergeability slot only needs "it's queued to
 *                          land".
 *
 * Unknown statuses fall through to a pass-through label so future
 * Graphite enums (e.g. merging) show up rather than silently
 * disappearing — we just don't know which colour tier yet.
 */
function mergeabilityLabel(
  m: MergeabilityEntry,
): { text: string; fg: string } | null {
  switch (m.status) {
    case "MERGEABLE":
      return { text: "mergeable", fg: theme.ok };
    case "FAILING_REQUIRED":
      return { text: "required checks failing", fg: theme.err };
    case "UNRESOLVED_COMMENTS":
      return { text: "unresolved comments", fg: theme.warn };
    case "QUEUED":
    case "QUEUED_TO_MERGE":
    case "RUNNING":
      return { text: "queued to merge", fg: theme.accent };
    case "DRAFT":
    case "CHANGES_REQUESTED":
    case "NEEDS_APPROVAL":
    case "NEEDS_APPROVALS":
    case "NEEDS_REVIEWERS":
      return null;
    default:
      return { text: m.status.toLowerCase().replace(/_/g, " "), fg: theme.info };
  }
}

/**
 * Build the PR row's segment list. Tiers picked so the PR id is sticky,
 * the mergeability state (next action) outranks ambient signals, and
 * carrots drop first because they're the noisiest line item.
 */
function buildPrSegments(
  pr: PullRequest,
  mergeability: MergeabilityEntry | undefined,
): Segment[] {
  const segs: Segment[] = [];
  const badge = prStateBadge(pr);
  const num = `#${pr.number}`;
  const numW = Bun.stringWidth(num);

  segs.push({
    key: "id",
    tier: 1,
    modes: [
      {
        width: 3 + numW,
        render: () => (
          <span fg={badge.fg}>
            {badge.glyph}  {num}
          </span>
        ),
      },
      { width: numW, render: () => <span fg={badge.fg}>{num}</span> },
      { width: 0, render: () => null },
    ],
  });

  if (mergeability && pr.state === "OPEN") {
    const label = mergeabilityLabel(mergeability);
    if (label) {
      segs.push({
        key: "mergeability",
        tier: 2,
        modes: [
          {
            width: 3 + Bun.stringWidth(label.text),
            render: () => (
              <span fg={label.fg}>
                {NF.mergeQueue}  {label.text}
              </span>
            ),
          },
          // Drop the prose first, keep just the glyph (color-coded) when
          // space gets tight — preserves the "something's blocking
          // merge" signal even in a narrow pane.
          {
            width: 2,
            render: () => <span fg={label.fg}>{NF.mergeQueue}</span>,
          },
          { width: 0, render: () => null },
        ],
      });
    }
  }

  if (pr.state === "OPEN") {
    const ck = checksLabel(pr.checks);
    if (ck) {
      segs.push({
        key: "checks",
        tier: 3,
        modes: [
          {
            width: 3 + Bun.stringWidth(ck.text),
            render: () => (
              <span fg={ck.fg}>
                {ck.glyph}  {ck.text}
              </span>
            ),
          },
          { width: 0, render: () => null },
        ],
      });
    }

    // Review before rabbit: human review is the primary signal, CR
    // is the supplementary "second review" — left-to-right reading
    // order mirrors that priority and the list-pane cluster's
    // [cr] [review] [pr] arrangement (which reads pr-first
    // right-to-left, putting review adjacent to the PR icon there).
    if (!pr.isDraft) {
      const rv = reviewLabel(pr.review);
      if (rv) {
        segs.push({
          key: "review",
          tier: 4,
          modes: [
            {
              width: 3 + Bun.stringWidth(rv.text),
              render: () => (
                <span fg={rv.fg}>
                  {rv.glyph}  {rv.text}
                </span>
              ),
            },
            { width: 0, render: () => null },
          ],
        });
      }
    }

    // Hide rabbit on drafts. Mirrors the review gate above for
    // symmetry — CR is essentially a second review, and on drafts CR's
    // "review skipped" outcome is currently misreported as "resting"
    // by the rollup.
    const rb = !pr.isDraft ? rabbitLabel(pr.rabbit) : null;
    if (rb) {
      const modes = [
        {
          width: 3 + Bun.stringWidth(rb.full),
          render: () => (
            <span fg={rb.fg}>
              {rb.glyph}  {rb.full}
            </span>
          ),
        },
      ];
      if (rb.tiny) {
        modes.push({
          width: 3 + Bun.stringWidth(rb.tiny),
          render: () => (
            <span fg={rb.fg}>
              {rb.glyph}  {rb.tiny}
            </span>
          ),
        });
      }
      modes.push({ width: 0, render: () => null });
      segs.push({ key: "rabbit", tier: 5, modes });
    }
  }

  return segs;
}

const PrLine = memo(function PrLine({
  row,
  valueWidth,
}: {
  row: WorktreeRow;
  valueWidth: number;
}) {
  const segments = useMemo(
    () => (row.pr ? buildPrSegments(row.pr, row.mergeability) : null),
    [row],
  );
  const fit = useMemo(
    () => (segments ? fitSegments(segments, valueWidth) : null),
    [segments, valueWidth],
  );
  if (!fit) return <text fg={theme.fgDim}>—</text>;
  return (
    <text fg={theme.fg} wrapMode="none" truncate>
      {fit.rendered}
    </text>
  );
});

export const prRow: RowModule = {
  id: "pr",
  label: "pr",
  sources: ({ github }) => [github],
  render: ({ row, valueWidth }) => <PrLine row={row} valueWidth={valueWidth} />,
};
