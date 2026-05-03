import { config } from "../../core/config.ts";
import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

/**
 * The branch this worktree is built on. Defaults to `config.branch.base`
 * (trunk) for un-stacked worktrees; switches to the parent worktree's
 * branch when commit-ancestry or PR-base signals fire (`row.stackedOn`).
 *
 * Always visible so the value is informative on every row — at-a-glance
 * confirmation that a branch targets trunk, or a clear pointer to the
 * parent when it doesn't. Future segments (rebase status, divergence
 * count, commit hash) can layer on without the row appearing/disappearing.
 *
 * A muted ` (pr)` suffix marks the case where the base came from the
 * PR's `baseRefName` rather than commit ancestry — i.e. GitHub thinks
 * we're stacked on this branch but our commits aren't actually built
 * on it (rebase pending, stale PR base). Mirrors the title-source
 * suffix style in `TitleLine`.
 */
export const baseRow: RowModule = {
  id: "base",
  label: "base",
  render: ({ row }) => {
    const base = row.stackedOn?.branch ?? config.branch.base;
    const fromPr = row.stackedOn?.via === "pr";
    return (
      <text wrapMode="none" truncate>
        <span fg={theme.fg}>{base}</span>
        {fromPr ? <span fg={theme.fgDim}> (pr)</span> : null}
      </text>
    );
  },
};
