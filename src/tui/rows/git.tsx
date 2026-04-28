import { humanAge } from "../../core/locks.ts";
import { StatusKind } from "../../core/types.ts";
import { statusBadge } from "../badges.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";
import { theme } from "../theme.ts";
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
  counts: { ahead: number; behind: number } | null;
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

function statusVerb(s: WorktreeRow["status"]): { glyph: string; text: string; fg: string; age?: string } {
  const badge = statusBadge(s);
  if (s.kind === StatusKind.Clean) return { ...badge, text: "clean" };
  if (s.kind === StatusKind.Busy) return { ...badge, text: s.label, age: s.age };
  return { ...badge, text: s.label };
}

function GitLine({ row }: { row: WorktreeRow }) {
  const verb = statusVerb(row.status);
  const sync = row.fields.sync.data;
  const ga = row.fields.gitActivity.data;
  const showSync = statusShowsSync(row.status.kind);
  // Activity stats are noise during a busy op or when the path is
  // missing — the line would shift around mid-operation.
  const showActivity =
    row.status.kind !== StatusKind.Busy && row.status.kind !== StatusKind.Missing;
  const now = Date.now();
  const diff = ga?.diff ?? null;
  const hasDiff = !!diff && (diff.added > 0 || diff.removed > 0);
  const commitAge =
    ga?.lastCommitMs != null ? ageMsToText(now - ga.lastCommitMs) : null;
  const createdAge =
    ga?.createdMs != null ? ageMsToText(now - ga.createdMs) : null;

  return (
    <text fg={theme.fg} wrapMode="none" truncate>
      <span fg={verb.fg}>
        {verb.glyph ? `${verb.glyph}  ` : ""}
        {verb.text}
        {verb.age ? <span> · {verb.age}</span> : null}
      </span>
      {showActivity && hasDiff && diff ? (
        <>
          <span fg={theme.fgDim}> · </span>
          <span fg={theme.warn}>+{diff.added}</span>
          <span> </span>
          <span fg={theme.err}>−{diff.removed}</span>
          {diff.files > 0 ? (
            <span fg={theme.fgDim}> ({diff.files} {diff.files === 1 ? "file" : "files"})</span>
          ) : null}
        </>
      ) : null}
      {showActivity && commitAge ? (
        <span fg={theme.fgDim}> · committed {commitAge}</span>
      ) : null}
      {showActivity && createdAge ? (
        <span fg={theme.fgDim}> · created {createdAge}</span>
      ) : null}
      {showSync ? (
        sync ? (
          <>
            <span fg={theme.fgDim}> · </span>
            <SyncGroup open="(" close=")" counts={sync.remote} />
            <span> </span>
            <SyncGroup open="[" close="]" counts={sync.main} />
          </>
        ) : (
          <span fg={theme.fgDim}>{" · …"}</span>
        )
      ) : null}
    </text>
  );
}

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
  render: ({ row }) => <GitLine row={row} />,
};
