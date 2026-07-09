import { TextAttributes } from "@opentui/core";

import type { RemovedWorktree } from "../../../core/wtstate.ts";
import { slugLabel } from "../../../core/stage.ts";
import { ageMsToText } from "../../text.ts";
import { NF } from "../../icons.ts";
import { theme } from "../../theme.ts";
import { RRRow } from "./row-cell.tsx";

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
 * Details body for a removed-worktree history entry. Everything here is
 * a snapshot taken at destroy time — no live worktree, no per-slug
 * sources, no AI pipeline — so it renders straight from the persisted
 * record. `⏎` restores, `p`/`i` open the PR/issue from the parent.
 */
export function RemovedBody({ entry }: { entry: RemovedWorktree }) {
  const removedMs = Date.parse(entry.removedAt);
  const removedText = Number.isFinite(removedMs)
    ? `${ageMsToText(Date.now() - removedMs)} ago · ${new Date(removedMs).toLocaleString()}`
    : null;
  const issueId = slugLabel(entry.slug).id;
  const pr = removedPrBadge(entry.prState);
  return (
    <box
      flexGrow={1}
      flexShrink={1}
      overflow="hidden"
      border
      borderStyle="single"
      borderColor={theme.border}
      title={` ${entry.slug} · removed `}
      titleAlignment="left"
      padding={1}
      flexDirection="column"
    >
      <box marginBottom={1}>
        <text wrapMode="word">
          <span fg={theme.fg} attributes={TextAttributes.BOLD}>
            {entry.title ?? entry.slug}
          </span>
        </text>
      </box>
      <RRRow label="branch">
        <text fg={theme.fg} wrapMode="none" truncate>
          {entry.branch}
        </text>
      </RRRow>
      {issueId ? (
        <RRRow label="issue">
          <text fg={theme.fg} wrapMode="none" truncate>
            {issueId}
          </text>
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
      {entry.prUrl ? (
        <box marginTop={1}>
          <text fg={theme.fgDim} wrapMode="none" truncate>
            {entry.prUrl}
          </text>
        </box>
      ) : null}
      <box flexGrow={1} flexShrink={1} minHeight={0} />
      <text fg={theme.fgDim} wrapMode="none" truncate>
        ⏎ restore worktree · h back
      </text>
    </box>
  );
}
