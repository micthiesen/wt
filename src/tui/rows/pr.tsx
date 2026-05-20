import { memo, useMemo } from "react";

import { isMergeQueued, type MergeabilityEntry } from "../../core/graphite-api.ts";
import type {
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
 *   - queued (`QUEUED` / `QUEUED_TO_MERGE` / `RUNNING`, via
 *                          `isMergeQueued`) — armed in the Graphite merge
 *                          queue, CI either in flight or awaiting its
 *                          turn. Collapsed to a single magenta `queued to
 *                          merge`; the adjacent checks badge already
 *                          conveys CI status. The list pane mirrors this
 *                          by overriding the PR glyph to the merge-queue
 *                          icon in the same magenta.
 *
 * Unknown statuses fall through to a pass-through label so future
 * Graphite enums (e.g. merging) show up rather than silently
 * disappearing — we just don't know which colour tier yet.
 */
function mergeabilityLabel(
  m: MergeabilityEntry,
): { text: string; fg: string } | null {
  if (isMergeQueued(m)) return { text: "queued to merge", fg: theme.info };
  switch (m.status) {
    case "MERGEABLE":
      return { text: "mergeable", fg: theme.ok };
    case "FAILING_REQUIRED":
      return { text: "required checks failing", fg: theme.err };
    case "UNRESOLVED_COMMENTS":
      return { text: "unresolved comments", fg: theme.warn };
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
