import type { RefObject } from "react";
import { TextAttributes } from "@opentui/core";
import type { ScrollBoxRenderable } from "@opentui/core";

import type { ReviewRequestPr } from "../../../core/github.ts";
import { useScrollbarNoFlash } from "../../hooks/useScrollbarNoFlash.ts";
import { ageMsToText } from "../../text.ts";
import { NF } from "../../icons.ts";
import { checkBadge, reviewBadge } from "../../badges.ts";
import { theme } from "../../theme.ts";
import { RRRow } from "./row-cell.tsx";

/** Map GitHub's `reviewDecision` to a glyph + color + human label. */
export function reviewDecisionBadge(
  d: ReviewRequestPr["reviewDecision"],
): { glyph: string; fg: string; label: string } | null {
  switch (d) {
    case "APPROVED": {
      const b = reviewBadge("approved");
      return b ? { ...b, label: "approved" } : null;
    }
    case "CHANGES_REQUESTED": {
      const b = reviewBadge("changes_requested");
      return b ? { ...b, label: "changes requested" } : null;
    }
    case "REVIEW_REQUIRED": {
      const b = reviewBadge("pending");
      return b ? { ...b, label: "review required" } : null;
    }
    default:
      return null;
  }
}

/**
 * Details body for a review-request PR. Not a worktree — no local
 * checkout, no per-slug sources, no AI summary pipeline — so it renders
 * straight from the PR search payload. Mirrors the worktree details
 * aesthetic: right-aligned labels, glyph-led values, and dense
 * `·`-separated lines (diff size, CI + review, ages) rather than one
 * stacked row per field. `p` opens it on GitHub from the parent; this
 * pane is read-only.
 */
export function ReviewRequestBody({
  pr,
  width: _width,
  scrollRef,
}: {
  pr: ReviewRequestPr;
  width: number;
  scrollRef?: RefObject<ScrollBoxRenderable | null>;
}) {
  const created = pr.createdAt ? Date.parse(pr.createdAt) : NaN;
  const updated = pr.updatedAt ? Date.parse(pr.updatedAt) : NaN;
  const openedText = Number.isFinite(created)
    ? `opened ${ageMsToText(Date.now() - created)} ago`
    : null;
  const updatedText =
    Number.isFinite(updated) && Number.isFinite(created) && updated !== created
      ? `updated ${ageMsToText(Date.now() - updated)} ago`
      : null;

  const sbRef = useScrollbarNoFlash(scrollRef);
  const check = checkBadge(pr.checks);
  const review = reviewDecisionBadge(pr.reviewDecision);
  const hasDiff = pr.additions > 0 || pr.deletions > 0 || pr.changedFiles > 0;
  return (
    <box
      flexGrow={1}
      flexShrink={1}
      overflow="hidden"
      border
      borderStyle="single"
      borderColor={theme.border}
      title={` ${pr.repoNameWithOwner}#${pr.number} `}
      titleAlignment="left"
      padding={1}
      flexDirection="column"
    >
      <scrollbox
        ref={sbRef}
        scrollY
        flexGrow={1}
        minHeight={0}
        contentOptions={{ flexDirection: "column" }}
      >
      <box marginBottom={1}>
        <text wrapMode="word">
          <span fg={theme.fg} attributes={TextAttributes.BOLD}>{pr.title}</span>
        </text>
      </box>
      <RRRow label="state">
        <text fg={pr.isDraft ? theme.fgDim : theme.accentAlt} wrapMode="none">
          {`${pr.isDraft ? NF.prDraft : NF.prOpen}  ${pr.isDraft ? "draft" : "ready"}`}
        </text>
      </RRRow>
      {pr.headRefName ? (
        <RRRow label="branch">
          <text fg={theme.fg} wrapMode="none" truncate>
            {pr.headRefName}
          </text>
        </RRRow>
      ) : null}
      {pr.author ? (
        <RRRow label="author">
          <text fg={theme.fg} wrapMode="none" truncate>
            {`@${pr.author}`}
          </text>
        </RRRow>
      ) : null}
      {hasDiff ? (
        <RRRow label="diff">
          <text wrapMode="none" truncate>
            <span fg={theme.warn}>{`+${pr.additions}`}</span>
            <span> </span>
            <span fg={theme.err}>{`−${pr.deletions}`}</span>
            {pr.changedFiles > 0 ? (
              <span fg={theme.fgDim}>
                {` · ${pr.changedFiles} ${pr.changedFiles === 1 ? "file" : "files"}`}
              </span>
            ) : null}
            {pr.commentCount > 0 ? (
              <span fg={theme.fgDim}>{` · ${NF.comment}  ${pr.commentCount}`}</span>
            ) : null}
          </text>
        </RRRow>
      ) : null}
      {check || review ? (
        <RRRow label="status">
          <text wrapMode="none">
            {check ? (
              <span fg={check.fg}>{`${check.glyph}  ${pr.checks === "pass" ? "passing" : pr.checks === "fail" ? "failing" : "pending"}`}</span>
            ) : null}
            {check && review ? <span fg={theme.fgDim}>{" · "}</span> : null}
            {review ? (
              <span fg={review.fg}>{`${review.glyph}  ${review.label}`}</span>
            ) : null}
          </text>
        </RRRow>
      ) : null}
      {openedText ? (
        <RRRow label="age">
          <text fg={theme.fgDim} wrapMode="none" truncate>
            {updatedText ? `${openedText} · ${updatedText}` : openedText}
          </text>
        </RRRow>
      ) : null}
      <box marginTop={1}>
        <text fg={theme.fgDim} wrapMode="none" truncate>
          {pr.url}
        </text>
      </box>
      </scrollbox>
    </box>
  );
}
