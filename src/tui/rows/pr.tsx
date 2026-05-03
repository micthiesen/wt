import { memo, useMemo } from "react";

import type {
  MergeQueueEntry,
  MergeQueueState,
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
 * Review badge mapping. Glyph SHAPE varies by state (thumbs up/down
 * for the verdict-bearing states, hourglass for pending, eye for "no
 * reviewers requested") so color isn't load-bearing — a colorblind
 * reader can still read the state. Distinct family from CI checks
 * (`checkPass/Fail/Pend`) so the two signals don't visually collide.
 */
function reviewLabel(r: PrReview): { glyph: string; text: string; fg: string } | null {
  switch (r) {
    case "approved":
      return { glyph: NF.thumbsUp, text: "approved", fg: theme.ok };
    case "changes_requested":
      return { glyph: NF.thumbsDown, text: "changes requested", fg: theme.err };
    case "pending":
      return { glyph: NF.hourglass, text: "review pending", fg: theme.warn };
    case "unrequested":
      return { glyph: NF.eye, text: "no reviewers", fg: theme.fgDim };
    default:
      return null;
  }
}

/**
 * CodeRabbit badge mapping. Single carrot glyph, color-coded — fits
 * the existing "carrots / grazing / resting" vocab and stays visually
 * distinct from human review (thumbs/hourglass/eye) and CI checks
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
    case "unresolved":
      return {
        glyph: NF.carrot,
        full: pluralize(rb.unresolved, "carrot"),
        tiny: String(rb.unresolved),
        fg: theme.warn,
      };
    case "pending":
      return { glyph: NF.carrot, full: "grazing", tiny: "", fg: theme.warn };
    case "clean":
      return { glyph: NF.carrot, full: "resting", tiny: "", fg: theme.ok };
    default:
      return null;
  }
}

function mqStateLabel(state: MergeQueueState): { text: string; fg: string } {
  switch (state) {
    case "MERGEABLE":
      return { text: "mergeable", fg: theme.ok };
    case "AWAITING_CHECKS":
      return { text: "awaiting checks", fg: theme.warn };
    case "QUEUED":
      return { text: "queued", fg: theme.warn };
    case "UNMERGEABLE":
      return { text: "unmergeable", fg: theme.err };
    case "LOCKED":
      return { text: "locked", fg: theme.err };
    default:
      return { text: state, fg: theme.fgDim };
  }
}

/**
 * Build the PR row's segment list. Tiers picked so the PR id is sticky,
 * the merge-queue state (next action) outranks ambient signals, and
 * carrots drop first because they're the noisiest line item.
 */
function buildPrSegments(pr: PullRequest, mq: MergeQueueEntry | undefined): Segment[] {
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

  if (mq) {
    const label = mqStateLabel(mq.state);
    const full = `queue #${mq.position} ${label.text}`;
    const mid = `queue #${mq.position}`;
    const tiny = `#${mq.position}`;
    segs.push({
      key: "queue",
      tier: 2,
      modes: [
        {
          width: 3 + Bun.stringWidth(full),
          render: () => (
            <span fg={label.fg}>
              {NF.mergeQueue}  {full}
            </span>
          ),
        },
        {
          width: 3 + Bun.stringWidth(mid),
          render: () => (
            <span fg={label.fg}>
              {NF.mergeQueue}  {mid}
            </span>
          ),
        },
        {
          width: 3 + Bun.stringWidth(tiny),
          render: () => (
            <span fg={label.fg}>
              {NF.mergeQueue}  {tiny}
            </span>
          ),
        },
        { width: 0, render: () => null },
      ],
    });
  } else if (pr.autoMerge && pr.state === "OPEN") {
    // Same slot as the queue segment — mutually exclusive in practice.
    // Dimmer color than `queue mergeable` since auto-merge is "armed
    // but idle" (waiting on preconditions) rather than actively
    // advancing.
    const full = "auto-merge";
    const tiny = "auto";
    segs.push({
      key: "queue",
      tier: 2,
      modes: [
        {
          width: 3 + Bun.stringWidth(full),
          render: () => (
            <span fg={theme.info}>
              {NF.mergeQueue}  {full}
            </span>
          ),
        },
        {
          width: 3 + Bun.stringWidth(tiny),
          render: () => (
            <span fg={theme.info}>
              {NF.mergeQueue}  {tiny}
            </span>
          ),
        },
        { width: 0, render: () => null },
      ],
    });
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
    () => (row.pr ? buildPrSegments(row.pr, row.mq) : null),
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
