import { pluralize } from "../../../core/text.ts";
import { rebaseBadge } from "../../badges.ts";
import type { WorktreeRow } from "../../hooks/useWorktreeRows.ts";
import { theme } from "../../theme.ts";

/** Conflicting files listed before collapsing into a `+N more` tail. */
const MAX_FILES = 8;

/**
 * Rebase-lifecycle block — the details-pane twin of the list cluster's
 * rebase slot (`rebaseBadge` drives both, so they can't drift). Sits
 * between the definition rows and the AI summary: the conflict state
 * carries a file LIST, which never fit the one-line definition format,
 * so the whole lifecycle renders as a block instead. Three states, in
 * the badge's priority order:
 *
 *  - **restacking**: the engine holds this worktree's `restack` lock —
 *    reconcile/replay running across its chain.
 *  - **rebasing**: the worktree sits mid-rebase (a `/restack` or hand
 *    rebase resolving a conflict — the engine's own bail aborts clean,
 *    so this appears once the resolving rebase starts).
 *  - **conflict**: the pre-flight `merge-tree` dry-run of HEAD against
 *    the effective base conflicts. Header names the base + count, then
 *    the clashing files one per line (capped at MAX_FILES). merge-tree
 *    is a merge, not a rebase replay, so for a multi-commit branch this
 *    is a strong hint, not a guarantee — `/restack` (or `R`) is still
 *    what actually resolves it.
 *
 * A clean or indeterminate probe renders nothing. A probe error (after
 * retries exhaust) renders verbatim — the driver-level `firstError`
 * gate doesn't apply here since this isn't a `RowModule`, so the block
 * carries its own, with the same `error && !isFetching` shape.
 */
export function RebaseBlock({ row }: { row: WorktreeRow }) {
  const lock = row.fields.lock;
  const conflict = row.fields.conflict;
  const probeError =
    conflict.error && !conflict.isFetching ? conflict.error : null;
  const badge = rebaseBadge(lock.data, conflict.data);
  if (!badge && !probeError) return null;
  if (probeError || !badge) {
    return (
      <box marginTop={1}>
        <text fg={theme.err} wrapMode="word">
          {probeError?.message ?? ""}
        </text>
      </box>
    );
  }
  const data = conflict.data;
  if (lock.data?.op === "restack") {
    return (
      <box marginTop={1}>
        <text fg={badge.fg} wrapMode="none" truncate>
          {`${badge.glyph}  Restacking (${lock.data.phase || "running"})`}
        </text>
      </box>
    );
  }
  if (data?.status === "rebasing") {
    return (
      <box marginTop={1}>
        <text fg={badge.fg} wrapMode="word">
          {`${badge.glyph}  Mid-rebase — resolve + continue in this worktree (/restack)`}
        </text>
      </box>
    );
  }
  if (data?.status !== "conflict") return null;
  // Friendly base: drop the `origin/` prefix on trunk.
  const base = data.base.replace(/^origin\//, "");
  const shown = data.files.slice(0, MAX_FILES);
  const extra = data.files.length - shown.length;
  return (
    <box marginTop={1} flexDirection="column">
      <text fg={badge.fg} wrapMode="none" truncate>
        {data.files.length > 0
          ? `${badge.glyph}  Won't rebase cleanly onto ${base} · ${pluralize(data.files.length, "conflicting file")}`
          : `${badge.glyph}  Won't rebase cleanly onto ${base}`}
      </text>
      {shown.map((f) => (
        <text key={f} fg={theme.fgDim} wrapMode="none" truncate>
          {`   ${f}`}
        </text>
      ))}
      {extra > 0 ? (
        <text fg={theme.fgDim} wrapMode="none" truncate>
          {`   +${extra} more`}
        </text>
      ) : null}
    </box>
  );
}
