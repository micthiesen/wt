import { TextAttributes } from "@opentui/core";

import { StatusKind } from "../../../core/types.ts";
import type { HarnessId } from "../../../core/harness/index.ts";
import type { DerivedState } from "../../../core/harness/status.ts";
import { STACK_CONNECTOR, stackOrdinalLabel } from "../../../core/stack-layout.ts";
import type { WorktreeRow } from "../../hooks/useWorktreeRows.ts";
import { statusBadge } from "../../badges.ts";
import { BadgeCluster } from "../../badge-cluster.tsx";
import { laneColor, theme } from "../../theme.ts";

/**
 * What the detail pane shows when a FOLDED section header is the cursor: the
 * stack/section overview. Built by `tui/hooks/useSectionDetail.ts` from the
 * folded section item's rows, so this pane stays free of state reads.
 */
export type SectionMember = {
  /** Same label the list row shows (`rowLabel`), so the folded summary
   *  and the expanded rows read identically. */
  label: string;
  /** The live list row — status/archived plus everything the shared
   *  badge cluster reads (pr, mq, deploy), including `row.stack` for
   *  the spine glyphs. */
  row: WorktreeRow;
  /** Badge-cluster inputs the list pane computes per slug (action
   *  glyph, harness session glyph + tint), passed through so the
   *  folded summary shows the identical cluster. */
  actionRunning: boolean;
  activeHarnessId: HarnessId | undefined;
  sessionState: DerivedState | undefined;
};

export type SectionDetail = {
  /** Stable section identity — keys the body so an AI-title label change
   *  doesn't remount the pane under a stationary cursor. */
  sectionKey: string;
  isStack: boolean;
  label: string;
  members: SectionMember[];
  /** Whole-stack automations pause (Ctrl+A); always false for manual sections. */
  automationsPaused: boolean;
};

/** The stack chain (spine · ordinal · status · title · badges). Rows
 *  arrive in list order carrying their `row.stack` layout info, so the
 *  lane order, connector glyphs, and ordinal labels match the expanded
 *  list gutter exactly; the right side renders the shared list-pane
 *  badge cluster. */
function StackChain({ members }: { members: SectionMember[] }) {
  // Status breakdown in StatusKind declaration order, non-zero kinds
  // only — the kind values double as display words ("dirty", "clean").
  const breakdown = Object.values(StatusKind)
    .map((k) => ({
      k,
      n: members.filter((m) => m.row.status.kind === k).length,
    }))
    .filter(({ n }) => n > 0)
    .map(({ k, n }) => `${n} ${k}`)
    .join(" · ");
  return (
    <>
      <text fg={theme.fgDim} wrapMode="none" truncate>
        {breakdown || "no worktrees"}
      </text>
      <box height={1} flexShrink={0} />
      {members.map((m) => {
        const info = m.row.stack;
        const b = statusBadge(m.row.status);
        return (
          <box key={m.row.wt.slug} flexDirection="row">
            {/* The lead glyphs must never shrink — when the row overflows,
                yoga squeezing these texts garbles the spine; the title is
                the only flexible (truncating) segment. */}
            <box flexShrink={0} flexDirection="row">
              <text fg={laneColor(info?.lane ?? 0)} wrapMode="none">
                {STACK_CONNECTOR[info?.pos ?? "single"]}
              </text>
              <text fg={theme.fgDim} wrapMode="none">{`${stackOrdinalLabel(info?.ordinal ?? 0)} `}</text>
              <text fg={b.fg} wrapMode="none">{`${b.glyph} `}</text>
            </box>
            {/* Flex-grow + overflow-hidden gives the title a bounded width so
                `truncate` ellipsises a long title instead of overflowing
                the row and garbling the pane. */}
            <box flexGrow={1} flexShrink={1} overflow="hidden">
              <text fg={theme.fg} wrapMode="none" truncate>{m.row.title}</text>
            </box>
            <BadgeCluster
              row={m.row}
              actionRunning={m.actionRunning}
              activeHarnessId={m.activeHarnessId}
              sessionState={m.sessionState}
            />
          </box>
        );
      })}
    </>
  );
}

/** The manual-section member list (status · label · badges), mirroring
 *  the StackChain row format minus the spine — manual members have no
 *  dependency relationships, so there's no tree to draw. The right side
 *  is the shared list-pane badge cluster, identical per row. */
function SectionMembers({ members }: { members: SectionMember[] }) {
  // Status breakdown in StatusKind declaration order, non-zero kinds
  // only — the kind values double as display words ("dirty", "clean").
  const breakdown = Object.values(StatusKind)
    .map((k) => ({
      k,
      n: members.filter((m) => m.row.status.kind === k).length,
    }))
    .filter(({ n }) => n > 0)
    .map(({ k, n }) => `${n} ${k}`)
    .join(" · ");
  return (
    <>
      <text fg={theme.fgDim} wrapMode="none" truncate>
        {breakdown || "no worktrees"}
      </text>
      <box height={1} flexShrink={0} />
      {members.map((m) => {
        const b = statusBadge(m.row.status);
        const dim = m.row.archived;
        return (
          <box key={m.row.wt.slug} flexDirection="row">
            <box width={2} flexShrink={0}>
              <text fg={dim ? theme.fgDim : b.fg} wrapMode="none">{b.glyph}</text>
            </box>
            <box width={1} flexShrink={0}>
              <text> </text>
            </box>
            <box flexGrow={1} flexShrink={1} overflow="hidden">
              <text
                fg={dim ? theme.fgDim : theme.fg}
                wrapMode="none"
                truncate
              >
                {m.label}
              </text>
            </box>
            <BadgeCluster
              row={m.row}
              actionRunning={m.actionRunning}
              activeHarnessId={m.activeHarnessId}
              sessionState={m.sessionState}
            />
          </box>
        );
      })}
    </>
  );
}

/** Detail-pane body for a folded section header (stack or manual section). */
export function SectionSummaryBody({ section, width }: { section: SectionDetail; width: number }) {
  return (
    <box
      flexGrow={1}
      width={width}
      flexShrink={0}
      border
      borderStyle="single"
      borderColor={theme.border}
      title={section.isStack ? " stack " : " section "}
      titleAlignment="left"
      padding={1}
    >
      <box flexShrink={0} overflow="hidden">
        <text fg={theme.fgBright} attributes={TextAttributes.BOLD} wrapMode="none" truncate>
          {section.label}
        </text>
      </box>
      {section.automationsPaused ? (
        <box flexShrink={0} overflow="hidden" marginTop={1}>
          <text wrapMode="none" truncate>
            <span fg={theme.warn}>{"⏸ "}</span>
            <span fg={theme.fgDim}>
              automations paused for this stack (ctrl+a resumes)
            </span>
          </text>
        </box>
      ) : null}
      <box height={1} flexShrink={0} />
      {section.isStack ? (
        <StackChain members={section.members} />
      ) : (
        <SectionMembers members={section.members} />
      )}
      <box flexGrow={1} flexShrink={1} minHeight={0} />
      <text fg={theme.fgDim} wrapMode="none">TAB to expand</text>
    </box>
  );
}
