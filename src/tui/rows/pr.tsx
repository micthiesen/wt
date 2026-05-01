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

function reviewLabel(r: PrReview): { glyph: string; text: string; fg: string } | null {
  switch (r) {
    case "approved":
      return { glyph: NF.checkPass, text: "approved", fg: theme.ok };
    case "changes_requested":
      return { glyph: NF.checkFail, text: "changes requested", fg: theme.err };
    case "pending":
      return { glyph: NF.checkPend, text: "review pending", fg: theme.warn };
    case "unrequested":
      return { glyph: NF.checkPend, text: "no reviewers", fg: theme.fgDim };
    default:
      return null;
  }
}

function rabbitLabel(
  rb: RabbitStatus,
): { glyph: string; full: string; tiny: string; fg: string } | null {
  switch (rb.state) {
    case "unresolved":
      return {
        glyph: NF.checkFail,
        full: pluralize(rb.unresolved, "carrot"),
        tiny: String(rb.unresolved),
        fg: theme.warn,
      };
    case "pending":
      return { glyph: NF.checkPend, full: "grazing", tiny: "", fg: theme.warn };
    case "clean":
      return { glyph: NF.checkPass, full: "resting", tiny: "", fg: theme.ok };
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

    const rb = rabbitLabel(pr.rabbit);
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
