import { pluralize } from "../../core/text.ts";
import { rebaseBadge } from "../badges.ts";
import type { RowModule } from "./types.ts";

/**
 * Rebase-lifecycle row — the details-pane twin of the list cluster's
 * rebase slot (`rebaseBadge` drives both, so they can't drift). Three
 * states render, in the badge's priority order:
 *
 *  - **restacking**: the engine holds this worktree's `restack` lock —
 *    reconcile/replay running across its chain.
 *  - **rebasing**: the worktree sits mid-rebase (a `/restack` or hand
 *    rebase resolving a conflict — the engine's own bail aborts clean,
 *    so this appears once the resolving rebase starts).
 *  - **conflict**: the pre-flight `merge-tree` dry-run of HEAD against
 *    the effective base conflicts. Names the base and the clashing
 *    files, truncated to fit. merge-tree is a merge, not a rebase
 *    replay, so for a multi-commit branch this is a strong hint, not a
 *    guarantee — `/restack` (or `R`) is still what actually resolves it.
 *
 * A clean or indeterminate probe hides the row entirely, so the details
 * pane stays quiet unless something is moving or needs fixing.
 */
export const conflictRow: RowModule = {
  id: "conflict",
  label: "rebase",
  sources: ({ row }) => [row.fields.conflict, row.fields.lock],
  visible: ({ row }) =>
    rebaseBadge(row.fields.lock.data, row.fields.conflict.data) !== null,
  render: ({ row }) => {
    const badge = rebaseBadge(row.fields.lock.data, row.fields.conflict.data);
    if (!badge) return null;
    const lock = row.fields.lock.data;
    const data = row.fields.conflict.data;
    let detail: string;
    if (lock?.op === "restack") {
      detail = `restacking (${lock.phase || "running"})`;
    } else if (data?.status === "rebasing") {
      detail = "mid-rebase — resolve + continue here (/restack)";
    } else if (data?.status === "conflict") {
      // Friendly base: drop the `origin/` prefix on trunk.
      const base = data.base.replace(/^origin\//, "");
      const n = data.files.length;
      detail =
        n > 0
          ? `conflicts with ${base} · ${pluralize(n, "file")}: ${data.files.join(", ")}`
          : `conflicts with ${base}`;
    } else {
      return null;
    }
    return (
      <text fg={badge.fg} wrapMode="none" truncate>
        {`${badge.glyph}  ${detail}`}
      </text>
    );
  },
};
