import { config } from "../../core/config.ts";
import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

/**
 * The branch this worktree is built on. Defaults to `config.branch.base`
 * (trunk) for un-stacked worktrees; switches to the parent worktree's
 * branch when an explicit stack parent is recorded (`row.stackedOn`),
 * set via the stack chord or `wt stack apply`.
 *
 * A muted "(manual)" suffix flags the explicit parent so it's obvious
 * the base came from a recorded relationship, not inferred.
 */
export const baseRow: RowModule = {
  id: "base",
  label: "base",
  render: ({ row }) => {
    const stackedOn = row.stackedOn;
    const base = stackedOn?.branch ?? config.branch.base;
    const suffix = stackedOn ? " (manual)" : null;
    return (
      <text wrapMode="none" truncate>
        <span fg={theme.fg}>{base}</span>
        {suffix ? <span fg={theme.fgDim}>{suffix}</span> : null}
      </text>
    );
  },
};
