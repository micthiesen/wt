import { config } from "../../core/config.ts";
import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

/**
 * The branch this worktree is built on. Defaults to `config.branch.base`
 * (trunk) for un-stacked worktrees; switches to the parent branch when
 * this worktree has a recorded fork base (`row.stackedOn`, derived from
 * the slug's `baseBranch` — set by `wt new --base` / `wt base` / the
 * `b` picker). A muted "(forked)" suffix flags the non-trunk case.
 */
export const baseRow: RowModule = {
  id: "base",
  label: "base",
  render: ({ row }) => {
    const stackedOn = row.stackedOn;
    const base = stackedOn?.branch ?? config.branch.base;
    return (
      <text wrapMode="none" truncate>
        <span fg={theme.fg}>{base}</span>
        {stackedOn ? <span fg={theme.fgDim}> (forked)</span> : null}
      </text>
    );
  },
};
