/**
 * AI row — generalized from the original claude-specific row. Shows
 * the F12-target session's state for the row's worktree, with the
 * harness identity rendered via its glyph + color. Claude entries
 * surface their derived state (working / waiting / abandoned / idle);
 * Codex / OpenCode entries fall back to "live" / "dead" since they
 * don't expose a busy/idle registry yet.
 *
 * Empty state (no discoverable session on any harness): show
 * "Primary: <harness> · F12 to start" as a hint so the row stays
 * useful and reflects what TAB selected.
 *
 * Implementation note: the row pulls its data from
 * `useHarnessSessions` directly rather than the row aggregator's
 * `fields.claude` channel — the AI view is cross-harness while the
 * aggregator's field is claude-only. Keeping the source hook here
 * means swapping rows or adding/removing harnesses doesn't ripple
 * back into `useWorktreeRows`.
 */
import { getHarness } from "../../core/harness/index.ts";
import type { Worktree } from "../../core/types.ts";
import { STATE_FG } from "../claude-state.ts";
import { useHarnessSessions } from "../hooks/useHarnessSessions.ts";
import { usePrimaryHarness } from "../hooks/usePrimaryHarness.ts";
import { ageMsToText } from "../text.ts";
import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

function AiLine({ wt }: { wt: Worktree }) {
  const primary = usePrimaryHarness();
  const { f12Target } = useHarnessSessions(wt.slug, wt.path, primary);
  if (!f12Target) {
    const harness = getHarness(primary);
    return (
      <text fg={theme.fgDim} wrapMode="none" truncate>
        <span fg={harness.color}>{harness.glyph} </span>
        primary: {harness.label} · F12 to start
      </text>
    );
  }
  const harness = getHarness(f12Target.harnessId);
  const state = f12Target.extras.derivedState;
  const stateText = state ?? (f12Target.isLive ? "live" : "dead");
  const stateFg = state ? STATE_FG[state] : theme.fgDim;
  const ageText =
    f12Target.lastActiveMs !== null
      ? ageMsToText(Date.now() - f12Target.lastActiveMs)
      : null;
  const queued = f12Target.extras.queued;
  return (
    <text fg={theme.fg} wrapMode="none" truncate>
      <span fg={harness.color}>{harness.glyph}  </span>
      <span fg={stateFg}>{stateText}</span>
      <span fg={theme.fgDim}> · </span>
      <span fg={theme.fg}>{f12Target.displayName}</span>
      {ageText ? <span fg={theme.fgDim}> · {ageText}</span> : null}
      {queued > 0 ? (
        <span fg={theme.warn}>
          {" · "}
          {queued} queued
        </span>
      ) : null}
    </text>
  );
}

export const claudeRow: RowModule = {
  id: "claude",
  label: "ai",
  // The legacy fields.claude is still wired through for back-compat
  // but unused here. We could drop it from `sources` once nothing
  // else reads it, but pulling the dependency now would force a sync
  // change to `useWorktreeRows` for no behavior gain.
  sources: ({ row }) => [row.fields.claude],
  render: ({ row }) => <AiLine wt={row.wt} />,
};
