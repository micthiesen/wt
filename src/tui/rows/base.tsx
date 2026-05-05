import { config } from "../../core/config.ts";
import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

/**
 * The branch this worktree is built on. Defaults to `config.branch.base`
 * (trunk) for un-stacked worktrees; switches to the parent worktree's
 * branch when the stack-detection or PR-base signal fires
 * (`row.stackedOn`).
 *
 * A muted "(pr)" suffix flags PR-base hits that have no patch-id
 * overlap with HEAD — the diff falls through to the declared base and
 * may be inaccurate. Stack-detected parents render plain.
 */
export const baseRow: RowModule = {
  id: "base",
  label: "base",
  render: ({ row }) => {
    const stackedOn = row.stackedOn;
    const base = stackedOn?.branch ?? config.branch.base;
    const suffix = stackedOn?.via === "pr" ? " (pr)" : null;
    return (
      <text wrapMode="none" truncate>
        <span fg={theme.fg}>{base}</span>
        {suffix ? <span fg={theme.fgDim}>{suffix}</span> : null}
      </text>
    );
  },
};
