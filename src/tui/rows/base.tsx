import { config } from "../../core/config.ts";
import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

/**
 * The branch this worktree is built on. Defaults to `config.branch.base`
 * (trunk) for un-stacked worktrees; switches to the parent slice's
 * branch when this worktree is a stacked manifest slice (`row.stackedOn`,
 * derived from `wtState.stacks`).
 *
 * A muted "(stack)" suffix flags that the base comes from the stack
 * manifest rather than being trunk.
 */
export const baseRow: RowModule = {
  id: "base",
  label: "base",
  render: ({ row }) => {
    const stackedOn = row.stackedOn;
    const base = stackedOn?.branch ?? config.branch.base;
    const suffix = stackedOn ? " (stack)" : null;
    return (
      <text wrapMode="none" truncate>
        <span fg={theme.fg}>{base}</span>
        {suffix ? <span fg={theme.fgDim}>{suffix}</span> : null}
      </text>
    );
  },
};
