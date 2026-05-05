import { config } from "../../core/config.ts";
import type { StackedOn } from "../hooks/useWorktreeRows.ts";
import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

/**
 * Muted suffix that flags an out-of-sync stack relationship. Anything
 * other than commit-ancestry means three-dot diff against the parent
 * branch produces a wrong-shape result; the suffix is the user's cue
 * to rebase. Mirrors the title-source suffix style in `TitleLine`.
 *
 *   (pr)        — PR declares the base, but no patch-id overlap with
 *                 HEAD. The diff falls through to the PR base and is
 *                 likely inaccurate.
 *   (patch-id)  — Parent's commits exist in HEAD's history under
 *                 different SHAs (parent rebased after the child
 *                 branched). The diff base is rerouted to skip the
 *                 rebased copies; rebasing the child puts the stack
 *                 back in sync.
 */
function syncSuffix(s: StackedOn): string | null {
  if (s.via === "pr") return " (pr)";
  if (s.via === "patch-id") return " (patch-id)";
  return null;
}

/**
 * The branch this worktree is built on. Defaults to `config.branch.base`
 * (trunk) for un-stacked worktrees; switches to the parent worktree's
 * branch when the commit-ancestry, patch-id, or PR-base signal fires
 * (`row.stackedOn`).
 *
 * Always visible so the value is informative on every row — at-a-glance
 * confirmation that a branch targets trunk, or a clear pointer to the
 * parent when it doesn't. Future segments (rebase status, divergence
 * count, commit hash) can layer on without the row appearing/disappearing.
 */
export const baseRow: RowModule = {
  id: "base",
  label: "base",
  render: ({ row }) => {
    const stackedOn = row.stackedOn;
    const base = stackedOn?.branch ?? config.branch.base;
    const suffix = stackedOn ? syncSuffix(stackedOn) : null;
    return (
      <text wrapMode="none" truncate>
        <span fg={theme.fg}>{base}</span>
        {suffix ? <span fg={theme.fgDim}>{suffix}</span> : null}
      </text>
    );
  },
};
