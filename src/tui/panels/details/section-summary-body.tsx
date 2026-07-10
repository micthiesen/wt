import { TextAttributes } from "@opentui/core";

import { StatusKind } from "../../../core/types.ts";
import type { HarnessId } from "../../../core/harness/index.ts";
import type { DerivedState } from "../../../core/harness/status.ts";
import type { StackManifest } from "../../../core/wtstate.ts";
import {
  layoutStack,
  STACK_CONNECTOR,
  stackOrdinalLabel,
  type SpinePos,
} from "../../../core/stack-layout.ts";
import type { WorktreeRow } from "../../hooks/useWorktreeRows.ts";
import { statusBadge } from "../../badges.ts";
import { BadgeCluster } from "../../badge-cluster.tsx";
import { laneColor, theme } from "../../theme.ts";

/**
 * What the detail pane shows when a FOLDED section header is the cursor: the
 * stack/section overview. Built by `tui/hooks/useSectionDetail.ts` from the folded section item +
 * the live manifest, so this pane stays free of state reads.
 */
export type SectionMember = {
  /** Same label the list row shows (`rowLabel`), so the folded summary
   *  and the expanded rows read identically. */
  label: string;
  /** The live list row — status/archived plus everything the shared
   *  badge cluster reads (pr, mq, deploy). */
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
  manifest: StackManifest | null;
  members: SectionMember[];
  /** Whole-stack automations pause (Ctrl+A); always false for manual sections. */
  automationsPaused: boolean;
};

/** Status glyph + color for one slice in the folded-stack summary. */
function sliceGlyph(status: StackManifest["slices"][number]["status"]): {
  t: string;
  fg: string;
} {
  if (status === "merged") return { t: "✓", fg: theme.ok };
  if (status === "open") return { t: "○", fg: theme.warn };
  return { t: "·", fg: theme.fgDim };
}

/** The stack chain (spine · ordinal · status · title · badges), like
 *  `wt stack status`. Rows come from `layoutStack` so the lane order,
 *  connector glyphs, and ordinal labels match the expanded list gutter
 *  exactly; the right side renders the shared list-pane badge cluster
 *  for slices with a live worktree (matched by branch), falling back
 *  to the dim PR number for slices without one (planned, or merged +
 *  cleaned). */
function StackChain({
  manifest,
  members,
}: {
  manifest: StackManifest;
  members: SectionMember[];
}) {
  const memberByBranch = new Map(members.map((m) => [m.row.wt.branch, m]));
  const count = (s: StackManifest["slices"][number]["status"]) =>
    manifest.slices.filter((x) => x.status === s).length;
  const nodes = layoutStack(manifest).nodes;
  // layoutStack degrades gracefully on a malformed manifest (cycle /
  // dangling parent) by dropping the affected slices; append those flat
  // so the summary still lists every slice.
  const laidOut = new Set(nodes.map((n) => n.slice.id));
  const rows: { slice: StackManifest["slices"][number]; pos: SpinePos; lane: number }[] = [
    ...nodes.map((n) => ({ slice: n.slice, pos: n.pos, lane: n.lane })),
    ...manifest.slices
      .filter((s) => !laidOut.has(s.id))
      .sort((a, b) => a.ordinal - b.ordinal)
      .map((s) => ({ slice: s, pos: "single" as SpinePos, lane: 0 })),
  ];
  return (
    <>
      <text fg={theme.fgDim} wrapMode="none" truncate>
        {count("merged")} merged · {count("open")} open · {count("planned")} planned
      </text>
      <box height={1} flexShrink={0} />
      {rows.map(({ slice: s, pos, lane }) => {
        const g = sliceGlyph(s.status);
        const member = memberByBranch.get(s.branch);
        return (
          <box key={s.id} flexDirection="row">
            {/* The lead glyphs must never shrink — when the row overflows,
                yoga squeezing these texts garbles the spine; the title is
                the only flexible (truncating) segment. */}
            <box flexShrink={0} flexDirection="row">
              <text fg={laneColor(lane)} wrapMode="none">{STACK_CONNECTOR[pos]}</text>
              <text fg={theme.fgDim} wrapMode="none">{`${stackOrdinalLabel(s.ordinal)} `}</text>
              <text fg={g.fg} wrapMode="none">{`${g.t} `}</text>
            </box>
            {/* Flex-grow + overflow-hidden gives the title a bounded width so
                `truncate` ellipsises a long slice title instead of overflowing
                the row and garbling the pane. */}
            <box flexGrow={1} flexShrink={1} overflow="hidden">
              <text fg={theme.fg} wrapMode="none" truncate>{s.title}</text>
            </box>
            {member ? (
              <BadgeCluster
                row={member.row}
                actionRunning={member.actionRunning}
                activeHarnessId={member.activeHarnessId}
                sessionState={member.sessionState}
              />
            ) : s.pr ? (
              <box flexShrink={0}>
                <text fg={theme.fgDim} wrapMode="none">{` #${s.pr}`}</text>
              </box>
            ) : null}
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
      {section.manifest ? (
        <StackChain manifest={section.manifest} members={section.members} />
      ) : (
        <SectionMembers members={section.members} />
      )}
      <box flexGrow={1} flexShrink={1} minHeight={0} />
      <text fg={theme.fgDim} wrapMode="none">TAB to expand</text>
    </box>
  );
}
