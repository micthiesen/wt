import { StatusKind } from "../../core/types.ts";
import { NF } from "../icons.ts";
import { theme } from "../theme.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";

type Props = {
  candidates: WorktreeRow[];
};

/**
 * Human-readable "why is this safe to clean" for a row. Always one of:
 * merged (git-level), gone (branch deleted upstream), PR merged (GitHub
 * says so even if local git hasn't caught up yet).
 */
function reasonFor(row: WorktreeRow): string {
  if (row.status.kind === StatusKind.Merged) return "merged";
  if (row.status.kind === StatusKind.Gone) return "gone";
  if (row.pr?.state === "MERGED") return "PR merged";
  return "—";
}

export function CleanConfirmModal({ candidates }: Props) {
  const count = candidates.length;
  const stageCount = candidates.filter((r) => r.fields.deploy.data).length;

  return (
    <box
      position="absolute"
      top="15%"
      left="15%"
      right="15%"
      bottom="15%"
      zIndex={10}
      backgroundColor={theme.bg}
      border
      borderStyle="double"
      borderColor={theme.warn}
      title={` clean · ${count} worktree${count === 1 ? "" : "s"} `}
      titleAlignment="left"
      padding={1}
      flexDirection="column"
    >
      <box flexDirection="column" marginBottom={1}>
        <text fg={theme.fg}>
          About to destroy{" "}
          <span fg={theme.accent} attributes={1}>
            {count}
          </span>{" "}
          worktree{count === 1 ? "" : "s"}
          {stageCount > 0 ? (
            <>
              {" · "}
              <span fg={theme.warn}>{stageCount}</span> stage
              {stageCount === 1 ? "" : "s"}
            </>
          ) : null}
          . Branches will be deleted.
        </text>
      </box>
      <box flexDirection="column" flexGrow={1} overflow="hidden">
        {candidates.map((row) => {
          const deployed = row.fields.deploy.data ?? false;
          return (
            <box key={row.wt.slug} flexDirection="row">
              <box width={2} flexShrink={0}>
                <text fg={theme.fgDim}>·</text>
              </box>
              <box flexGrow={1} flexShrink={1} overflow="hidden">
                <text fg={theme.fg} wrapMode="none" truncate>
                  {row.wt.slug}
                </text>
              </box>
              <box flexShrink={0} flexDirection="row">
                <text fg={theme.fgDim}>{"  "}</text>
                <text fg={theme.fgDim}>{reasonFor(row).padEnd(10)}</text>
                {deployed ? (
                  // Two spaces after the bolt: opentui's native renderer
                  // treats the PUA codepoint as 1-cell wide so a single
                  // space leaves "destroys" overlapping the icon's right
                  // half. Same pattern as `ChecksBadge` in details.
                  <text fg={theme.warn}> {NF.bolt}  destroys stage</text>
                ) : (
                  <text fg={theme.fgDim}> no stage</text>
                )}
              </box>
            </box>
          );
        })}
      </box>
      <box flexDirection="row" marginTop={1}>
        <text fg={theme.accent} attributes={1}>
          y
        </text>
        <text fg={theme.fgDim}> confirm · </text>
        <text fg={theme.accent} attributes={1}>
          n / esc
        </text>
        <text fg={theme.fgDim}> cancel</text>
      </box>
    </box>
  );
}
