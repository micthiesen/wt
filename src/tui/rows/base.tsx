import { config } from "../../core/config.ts";
import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

/**
 * The branch this worktree is built on. Defaults to `config.branch.base`
 * (trunk) for un-stacked worktrees; switches to the parent branch when
 * this worktree is a stacked manifest slice or a recorded fork
 * (`row.stackedOn`, derived from `wtState.stacks` / slug `baseBranch`).
 *
 * A muted suffix flags where a non-trunk base came from: "(stack)" =
 * the stack manifest, "(forked)" = recorded by `wt new --base`.
 */
export const baseRow: RowModule = {
  id: "base",
  label: "base",
  render: ({ row }) => {
    const stackedOn = row.stackedOn;
    const base = stackedOn?.branch ?? config.branch.base;
    const suffix = stackedOn ? (stackedOn.via === "stack" ? " (stack)" : " (forked)") : null;
    return (
      <text wrapMode="none" truncate>
        <span fg={theme.fg}>{base}</span>
        {suffix ? <span fg={theme.fgDim}>{suffix}</span> : null}
      </text>
    );
  },
};
