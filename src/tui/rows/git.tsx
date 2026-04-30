import { memo, useMemo } from "react";

import type { GitActivity } from "../../core/git-activity.ts";
import { humanAge } from "../../core/locks.ts";
import { StatusKind } from "../../core/types.ts";
import type { SyncCounts, SyncState } from "../../core/worktree.ts";
import { statusBadge } from "../badges.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";
import { theme } from "../theme.ts";
import { fitSegments, type Segment } from "./fit.tsx";
import type { RowModule } from "./types.ts";

function ageMsToText(ms: number): string {
  return humanAge(Math.max(0, ms) / 1000);
}

/**
 * Sync info only makes sense for worktrees we can actually inspect —
 * hide during transient / unreadable states so the status line doesn't
 * flash/rearrange while a background op runs.
 */
function statusShowsSync(kind: StatusKind): boolean {
  return kind !== StatusKind.Busy && kind !== StatusKind.Missing;
}

function SyncGroup({
  open,
  close,
  counts,
}: {
  open: string;
  close: string;
  counts: SyncCounts | null;
}) {
  if (counts === null) {
    return (
      <span fg={theme.fgDim}>
        {open}—{close}
      </span>
    );
  }
  const zero = counts.ahead === 0 && counts.behind === 0;
  if (zero) {
    return (
      <span fg={theme.fgDim}>
        {open}↑0 ↓0{close}
      </span>
    );
  }
  return (
    <>
      <span fg={theme.fgDim}>{open}</span>
      {counts.ahead > 0 ? <span fg={theme.warn}>↑{counts.ahead}</span> : null}
      {counts.ahead > 0 && counts.behind > 0 ? <span> </span> : null}
      {counts.behind > 0 ? <span fg={theme.err}>↓{counts.behind}</span> : null}
      <span fg={theme.fgDim}>{close}</span>
    </>
  );
}

function syncGroupWidth(counts: SyncCounts | null): number {
  if (counts === null) return 3; // "(—)"
  if (counts.ahead === 0 && counts.behind === 0) return 7; // "(↑0 ↓0)"
  if (counts.ahead > 0 && counts.behind > 0) {
    // "(↑N ↓M)"
    return 5 + String(counts.ahead).length + String(counts.behind).length;
  }
  // "(↑N)" or "(↓M)"
  return 3 + String(counts.ahead || counts.behind).length;
}

type Verb = { glyph: string; text: string; fg: string; age?: string };

function statusVerb(s: WorktreeRow["status"]): Verb {
  const badge = statusBadge(s);
  if (s.kind === StatusKind.Clean) return { ...badge, text: "clean" };
  if (s.kind === StatusKind.Busy) return { ...badge, text: s.label, age: s.age };
  return { ...badge, text: s.label };
}

function verbWidth(v: Verb): number {
  // glyph + 2sp prefix when present, then text, then optional " · age".
  return (
    (v.glyph ? 3 : 0) +
    Bun.stringWidth(v.text) +
    (v.age ? 3 + Bun.stringWidth(v.age) : 0)
  );
}

function VerbView({ verb }: { verb: Verb }) {
  return (
    <span fg={verb.fg}>
      {verb.glyph ? `${verb.glyph}  ` : ""}
      {verb.text}
      {verb.age ? <span> · {verb.age}</span> : null}
    </span>
  );
}

/**
 * Build the row's segment list. Tiers are picked so the verb is sticky,
 * meaningful change-state (diff + sync) outranks ages, and "created"
 * drops before "committed" — most users care more about recency than
 * branch age once both have been compacted to bare values.
 */
function buildGitSegments(row: WorktreeRow): Segment[] {
  const segs: Segment[] = [];
  const verb = statusVerb(row.status);
  const showActivity =
    row.status.kind !== StatusKind.Busy && row.status.kind !== StatusKind.Missing;
  const showSync = statusShowsSync(row.status.kind);
  const ga: GitActivity | undefined = row.fields.gitActivity.data;
  const sync: SyncState | undefined = row.fields.sync.data;

  segs.push({
    key: "verb",
    tier: 1,
    modes: [
      { width: verbWidth(verb), render: () => <VerbView verb={verb} /> },
      { width: 0, render: () => null },
    ],
  });

  const diff = ga?.diff ?? null;
  const hasDiff = !!diff && (diff.added > 0 || diff.removed > 0);
  if (showActivity && hasDiff && diff) {
    const adds = String(diff.added);
    const rems = String(diff.removed);
    // "+N −M" base width: "+N" + " " + "−M" = 1+adds + 1 + 1+rems.
    const baseW = 3 + adds.length + rems.length;
    // " (K files)" or " (K file)".
    const filesSuffix =
      diff.files > 0
        ? ` (${diff.files} ${diff.files === 1 ? "file" : "files"})`
        : "";
    const fullW = baseW + Bun.stringWidth(filesSuffix);
    const renderCounts = (
      <>
        <span fg={theme.warn}>+{diff.added}</span>
        <span> </span>
        <span fg={theme.err}>−{diff.removed}</span>
      </>
    );
    segs.push({
      key: "diff",
      tier: 2,
      modes: [
        {
          width: fullW,
          render: () => (
            <>
              {renderCounts}
              {filesSuffix ? (
                <span fg={theme.fgDim}>{filesSuffix}</span>
              ) : null}
            </>
          ),
        },
        { width: baseW, render: () => renderCounts },
        { width: 0, render: () => null },
      ],
    });
  }

  if (showActivity && ga?.lastCommitMs != null) {
    const age = ageMsToText(Date.now() - ga.lastCommitMs);
    const fullText = `committed ${age}`;
    segs.push({
      key: "commit",
      tier: 3,
      modes: [
        {
          width: Bun.stringWidth(fullText),
          render: () => <span fg={theme.fgDim}>{fullText}</span>,
        },
        {
          width: Bun.stringWidth(age),
          render: () => <span fg={theme.fgDim}>{age}</span>,
        },
        { width: 0, render: () => null },
      ],
    });
  }

  if (showActivity && ga?.createdMs != null) {
    const age = ageMsToText(Date.now() - ga.createdMs);
    const fullText = `created ${age}`;
    const compact = `+${age}`;
    segs.push({
      key: "created",
      tier: 4,
      modes: [
        {
          width: Bun.stringWidth(fullText),
          render: () => <span fg={theme.fgDim}>{fullText}</span>,
        },
        {
          width: Bun.stringWidth(compact),
          render: () => <span fg={theme.fgDim}>{compact}</span>,
        },
        { width: 0, render: () => null },
      ],
    });
  }

  if (showSync) {
    if (sync) {
      const remoteW = syncGroupWidth(sync.remote);
      const mainW = syncGroupWidth(sync.main);
      // Both groups joined with a single space — they're conceptually
      // the same question (sync state) so they compact together rather
      // than competing across tiers.
      segs.push({
        key: "sync",
        tier: 2,
        modes: [
          {
            width: remoteW + 1 + mainW,
            render: () => (
              <>
                <SyncGroup open="(" close=")" counts={sync.remote} />
                <span> </span>
                <SyncGroup open="[" close="]" counts={sync.main} />
              </>
            ),
          },
          {
            width: remoteW,
            render: () => <SyncGroup open="(" close=")" counts={sync.remote} />,
          },
          { width: 0, render: () => null },
        ],
      });
    } else {
      // Loading placeholder: a single dim glyph the user reads as
      // "fetching". No compaction beyond drop — it's already minimal.
      segs.push({
        key: "sync-pending",
        tier: 5,
        modes: [
          {
            width: 3,
            render: () => <span fg={theme.fgDim}>...</span>,
          },
          { width: 0, render: () => null },
        ],
      });
    }
  }

  return segs;
}

const GitLine = memo(function GitLine({
  row,
  valueWidth,
}: {
  row: WorktreeRow;
  valueWidth: number;
}) {
  // `row` is ref-stable when content is stable (see `useWorktreeRows`'
  // per-field reuse + row-equality short-circuit). So a single dep on
  // `row` invalidates the segment array iff something the renderer
  // would read has actually changed.
  const segments = useMemo(() => buildGitSegments(row), [row]);
  const fit = useMemo(
    () => fitSegments(segments, valueWidth),
    [segments, valueWidth],
  );
  return (
    <text fg={theme.fg} wrapMode="none" truncate>
      {fit.rendered}
    </text>
  );
});

export const gitRow: RowModule = {
  id: "git",
  label: "git",
  // Aggregates several per-worktree fetches. Order matters: the
  // first one to error wins the inline error slot. Lock first so a
  // failing lock read (the cheapest, most reliable signal) surfaces
  // ahead of richer-but-flakier git inspections.
  sources: ({ row }) => [
    row.fields.lock,
    row.fields.merged,
    row.fields.gone,
    row.fields.dirty,
    row.fields.sync,
    row.fields.gitActivity,
  ],
  render: ({ row, valueWidth }) => (
    <GitLine row={row} valueWidth={valueWidth} />
  ),
};
