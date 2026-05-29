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
import { checkBadge, prStateBadge, rabbitBadge, reviewBadge } from "../badges.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";
import { NF } from "../icons.ts";
import { theme } from "../theme.ts";
import { fitSegments, type Segment } from "./fit.tsx";
import type { RowModule } from "./types.ts";

// Glyph/color from `checkBadge`; this only adds the details-pane prose.
function checksLabel(c: PrChecks): { glyph: string; text: string; fg: string } | null {
  const badge = checkBadge(c);
  if (!badge) return null;
  return { ...badge, text: c === "pending" ? "checks pending" : "checks" };
}

// Glyph/color from `reviewBadge`; this only adds the details-pane prose.
function reviewLabel(r: PrReview): { glyph: string; text: string; fg: string } | null {
  const badge = reviewBadge(r);
  if (!badge) return null;
  const text =
    r === "approved"
      ? "approved"
      : r === "changes_requested"
        ? "changes requested"
        : r === "pending"
          ? "review pending"
          : "no reviewers";
  return { ...badge, text };
}

// Glyph/color from `rabbitBadge`; this only adds the details-pane
// `full`/`tiny` prose. Draft-hide lives at the `buildPrSegments` call
// site, not here.
function rabbitLabel(
  rb: RabbitStatus,
): { glyph: string; full: string; tiny: string; fg: string } | null {
  const badge = rabbitBadge(rb);
  if (!badge) return null;
  switch (rb.state) {
    case "unresolved":
      return {
        ...badge,
        full: pluralize(rb.unresolved, "carrot"),
        tiny: String(rb.unresolved),
      };
    case "pending":
      return { ...badge, full: "grazing", tiny: "" };
    case "clean":
      return { ...badge, full: "resting", tiny: "" };
    default:
      return null;
  }
}

/**
 * Merge-queue state label for the details-pane segment. Color tier
 * follows severity: green when mergeable, yellow while waiting on
 * checks / its turn, red when blocked. Unknown states pass through
 * verbatim (dim) so a new GitHub enum surfaces rather than vanishing.
 */
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
 * a real merge-queue entry (next action) outranks ambient signals, and
 * the auto-merge indicator drops first (tier 6) as the least load-
 * bearing signal — ahead of checks, review, and carrots.
 */
function buildPrSegments(
  pr: PullRequest,
  mq: MergeQueueEntry | undefined,
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
    // Occupies the queue slot (mutually exclusive with a real queue
    // entry) but ranks dead last — highest tier, so it compacts and
    // drops before checks, review, and rabbit. Auto-merge is "armed but
    // idle" (waiting on preconditions), the least load-bearing signal on
    // the line, so it's the first thing to yield when space is tight.
    // Dimmer color than `queue mergeable` for the same reason.
    const full = "auto-merge";
    const tiny = "auto";
    segs.push({
      key: "automerge",
      tier: 6,
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
