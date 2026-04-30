import type { MergeQueueEntry, MergeQueueState, PrReview, PullRequest } from "../../core/types.ts";
import { prStateBadge } from "../badges.ts";
import { NF } from "../icons.ts";
import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

function ChecksBadge({ pr }: { pr: PullRequest }) {
  if (pr.state !== "OPEN") return null;
  let body: React.ReactNode;
  switch (pr.checks) {
    case "pass":
      body = <span fg={theme.ok}>{NF.checkPass}  checks</span>;
      break;
    case "fail":
      body = <span fg={theme.err}>{NF.checkFail}  checks</span>;
      break;
    case "pending":
      body = <span fg={theme.warn}>{NF.checkPend}  checks pending</span>;
      break;
    default:
      return null;
  }
  return (
    <>
      <span fg={theme.fgDim}> · </span>
      {body}
    </>
  );
}

function reviewLabel(r: PrReview): { glyph: string; text: string; fg: string } | null {
  switch (r) {
    case "approved":
      return { glyph: NF.checkPass, text: "approved", fg: theme.ok };
    case "changes_requested":
      return { glyph: NF.checkFail, text: "changes requested", fg: theme.err };
    case "pending":
      return { glyph: NF.checkPend, text: "review pending", fg: theme.warn };
    default:
      return null;
  }
}

function ReviewBadge({ pr }: { pr: PullRequest }) {
  if (pr.state !== "OPEN") return null;
  // Drafts don't expect reviews; suppress to avoid a permanent
  // "pending" badge on every draft.
  if (pr.isDraft) return null;
  const r = reviewLabel(pr.review);
  if (!r) return null;
  return (
    <>
      <span fg={theme.fgDim}> · </span>
      <span fg={r.fg}>
        {r.glyph}  {r.text}
      </span>
    </>
  );
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

function QueueBadge({ mq }: { mq: MergeQueueEntry | undefined }) {
  if (!mq) return null;
  const label = mqStateLabel(mq.state);
  return (
    <>
      <span fg={theme.fgDim}> · </span>
      <span fg={label.fg}>{NF.mergeQueue}  queue #{mq.position} {label.text}</span>
    </>
  );
}

function RabbitBadge({ pr }: { pr: PullRequest }) {
  if (pr.state !== "OPEN") return null;
  let body: React.ReactNode;
  switch (pr.rabbit.state) {
    case "unresolved":
      body = (
        <span fg={theme.warn}>
          {NF.checkFail}  rabbit {pr.rabbit.unresolved} unresolved
        </span>
      );
      break;
    case "pending":
      body = <span fg={theme.warn}>{NF.checkPend}  rabbit pending</span>;
      break;
    case "clean":
      body = <span fg={theme.ok}>{NF.checkPass}  rabbit</span>;
      break;
    default:
      return null;
  }
  return (
    <>
      <span fg={theme.fgDim}> · </span>
      {body}
    </>
  );
}

export const prRow: RowModule = {
  id: "pr",
  label: "pr",
  sources: ({ github }) => [github],
  render: ({ row }) => {
    const { pr, mq } = row;
    if (!pr) return <text fg={theme.fgDim}>—</text>;
    const badge = prStateBadge(pr);
    return (
      <text fg={theme.fg} wrapMode="none" truncate>
        <span fg={badge.fg}>{badge.glyph}  #{pr.number}</span>
        <ChecksBadge pr={pr} />
        <RabbitBadge pr={pr} />
        <ReviewBadge pr={pr} />
        <QueueBadge mq={mq} />
      </text>
    );
  },
};
