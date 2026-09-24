import { isMergedRemoval, type RemovedWorktree } from "../../../core/wtstate.ts";
import { resolveIssueId } from "../../../core/issue-tracker.ts";
import { ageMsToText, truncateEnd } from "../../text.ts";
import { NF } from "../../icons.ts";
import { theme } from "../../theme.ts";
import { IssueLine } from "../../rows/issue.tsx";
import { RRRow } from "./row-cell.tsx";
import { detailPaneTitle, DetailTitleLine } from "./title.tsx";
import { WorkStatusRecordBlock } from "./work-status-block.tsx";

/** Glyph + label for a removed entry's snapshotted PR state. */
export function removedPrBadge(state: string | undefined): {
  glyph: string;
  fg: string;
  label: string;
} | null {
  switch (state) {
    case "MERGED":
      return { glyph: NF.prMerged, fg: theme.ok, label: "merged" };
    case "CLOSED":
      return { glyph: NF.prClosed, fg: theme.err, label: "closed" };
    case "OPEN":
      return { glyph: NF.prOpen, fg: theme.accentAlt, label: "open at removal" };
    default:
      return null;
  }
}

/**
 * Details body for a removed-worktree history entry. Identity, work,
 * PR, and outcome are removal snapshots; the optional tracker status
 * comes from the history view's existing batch, never a per-slug read.
 * `⏎` restores, `p`/`i` open the PR/issue from the parent.
 */
export function RemovedBody({ entry, width, issueStatus }: {
  entry: RemovedWorktree;
  width: number;
  issueStatus?: string;
}) {
  const removedMs = Date.parse(entry.removedAt);
  const removedText = Number.isFinite(removedMs)
    ? `${ageMsToText(Date.now() - removedMs)} ago · ${new Date(removedMs).toLocaleString()}`
    : null;
  const issueId = resolveIssueId(entry.slug, entry.issueId);
  const pr = removedPrBadge(entry.prState);
  return (
    <box
      flexGrow={1}
      flexShrink={1}
      overflow="hidden"
      border
      borderStyle="single"
      borderColor={theme.border}
      title={detailPaneTitle(entry.slug, width, " · removed")}
      titleAlignment="left"
      padding={1}
      flexDirection="column"
    >
      <DetailTitleLine title={entry.title ?? entry.slug} />
      {entry.work ? (
        <WorkStatusRecordBlock
          record={entry.work}
          contentWidth={Math.max(1, width - 4)}
          verifyExpanded={null}
          landed={isMergedRemoval(entry)}
          lastCommitMs={null}
        />
      ) : (
        <text fg={theme.fgDim}>status unknown</text>
      )}
      <RRRow label="branch">
        <text fg={theme.fg} wrapMode="none" truncate>
          {entry.branch}
        </text>
      </RRRow>
      {issueId || entry.githubIssue ? (
        <RRRow label="issue">
          <IssueLine id={issueId} githubIssue={entry.githubIssue} status={issueStatus} />
        </RRRow>
      ) : null}
      {entry.prNumber !== undefined ? (
        <RRRow label="pr">
          <text wrapMode="none" truncate>
            <span fg={pr?.fg ?? theme.fg}>
              {`${pr ? `${pr.glyph}  ` : ""}#${entry.prNumber}`}
            </span>
            {pr ? <span fg={theme.fgDim}>{` · ${pr.label}`}</span> : null}
          </text>
        </RRRow>
      ) : null}
      {removedText ? (
        <RRRow label="removed">
          <text fg={theme.fgDim} wrapMode="none" truncate>
            {removedText}
          </text>
        </RRRow>
      ) : null}
      {/* Same shape as the live pane's `AutomationsPausedLine`. It
          matters more here than there: a post-merge `external` run
          outlives the checkout, so this is the only surface that can
          say whether one is still going to fire for this slug. */}
      {entry.automationsPaused ? (
        <box marginTop={1}>
          <text wrapMode="none" truncate>
            <span fg={theme.warn}>{"⏸ "}</span>
            <span fg={theme.fgDim}>
              {"automations paused for this archived worktree (ctrl+a resumes)"}
            </span>
          </text>
        </box>
      ) : null}
      {entry.prUrl ? (
        <box marginTop={1}>
          <text fg={theme.fgDim} wrapMode="none" truncate>
            {entry.prUrl}
          </text>
        </box>
      ) : null}
      <box flexGrow={1} flexShrink={1} minHeight={0} />
      {/* Hand-rolled end-truncation, not opentui's native `truncate`:
          this line teaches keybinds, so a middle-clip silently deleting
          `p PR · i issue` from the middle is the worst possible failure
          mode. End-truncation at least drops from the tail (`h back`),
          which is the least critical hint. No scrollbox here (unlike
          the worktree details pane), so the budget is just border +
          padding on each side. */}
      <text fg={theme.fgDim} wrapMode="none">
        {truncateEnd(
          "⏎ restore · p PR · i issue · y yank · ctrl+a autos · h back",
          Math.max(0, width - 4),
        )}
      </text>
    </box>
  );
}
