import type { DerivedState } from "../../../core/harness/status.ts";
import { pluralize } from "../../../core/text.ts";
import { rebaseBadge } from "../../badges.ts";
import { NF } from "../../icons.ts";
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
 *  - **resolving**: the branch still conflicts but the row's active
 *    session is engaged — the conflict handoff (or a hand-typed
 *    `/restack`) is on it. Files still listed: they're what's being
 *    resolved.
 *  - **conflict**: the pre-flight `merge-tree` dry-run of HEAD against
 *    the effective base conflicts and nothing is working on it. Header
 *    names the base + count, then the clashing files one per line
 *    (capped at MAX_FILES). merge-tree is a merge, not a rebase replay,
 *    so for a multi-commit branch this is a strong hint, not a
 *    guarantee — `/restack` (or `R`) is still what actually resolves it.
 *
 * A clean or indeterminate probe renders nothing. A probe error (after
 * retries exhaust) renders verbatim — the driver-level `firstError`
 * gate doesn't apply here since this isn't a `RowModule`, so the block
 * carries its own, with the same `error && !isFetching` shape.
 */
export function RebaseBlock({
  row,
  sessionState,
}: {
  row: WorktreeRow;
  /** Derived state of the row's active session — flips conflict to
   *  "resolving" while engaged (same signal as the list cluster). */
  sessionState?: DerivedState;
}) {
  const lock = row.fields.lock;
  const conflict = row.fields.conflict;
  const probeError =
    conflict.error && !conflict.isFetching ? conflict.error : null;
  const badge = rebaseBadge(lock.data, conflict.data, sessionState);
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
  // The resolving variant keeps the conflict data but a sync glyph —
  // detect it off the badge rather than re-deriving the session check.
  const resolving = badge.glyph !== NF.conflict;
  // Friendly base: drop the `origin/` prefix on trunk.
  const base = data.base.replace(/^origin\//, "");
  const shown = data.files.slice(0, MAX_FILES);
  const extra = data.files.length - shown.length;
  const header = resolving
    ? `Resolving conflict with ${base} in the session`
    : `Won't rebase cleanly onto ${base}`;
  return (
    <box marginTop={1} flexDirection="column">
      <text fg={badge.fg} wrapMode="none" truncate>
        {data.files.length > 0
          ? `${badge.glyph}  ${header} · ${pluralize(data.files.length, "conflicting file")}`
          : `${badge.glyph}  ${header}`}
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
