/**
 * The one-row header: worktree counts, the refresh wave, the paused /
 * queued automations chip, and the usage + primary-harness badges.
 *
 * Extracted from App for a load-bearing reason, not tidiness: this is
 * the only place that watches the global in-flight query count, and
 * `useFetchingCount()` re-renders its component on EVERY fetch start and
 * finish anywhere in the cache. Living in App, that meant the entire
 * tree re-rendered dozens of times a second during refresh waves —
 * precisely when the board is busiest. Here the churn is contained to
 * this one-row component (and the wave it animates). Don't move
 * the fetch counter (or any other per-fetch observer) back up into App.
 */
import { memo } from "react";
import { useFetchingCount } from "../hooks/useFetchingCount.ts";

import { config } from "../../core/config.ts";
import type { HarnessId } from "../../core/harness/index.ts";
import { MAX_WAVE_WIDTH, RefreshWave } from "../spinner.tsx";
import { theme } from "../theme.ts";
import { PrimaryHarnessBadge, UsageBadge } from "../usage-badge.tsx";

type Props = {
  isLoading: boolean;
  activeCount: number;
  archivedCount: number;
  remoteUnavailable: boolean;
  remoteVersionMismatch: boolean;
  automationsConfigured: boolean;
  automationsPaused: boolean;
  automationsPending: number;
  primaryHarness: HarnessId;
};

export const TitleBar = memo(function TitleBar({
  isLoading,
  activeCount,
  archivedCount,
  remoteUnavailable,
  remoteVersionMismatch,
  automationsConfigured,
  automationsPaused,
  automationsPending,
  primaryHarness,
}: Props) {
  // Global in-flight count — covers the root queries and the imperative
  // `fetchOriginQuery` call, not just the observed per-worktree fields.
  // Using the per-row aggregate alone made the indicator flash briefly
  // at the tail of a refresh (after `git fetch origin` resolved) instead
  // of lighting up for the whole window.
  const fetchingCount = useFetchingCount();
  const loadingNote = isLoading ? " · loading..." : "";
  const archivedNote = archivedCount > 0 ? ` · ${archivedCount} archived` : "";
  const titleBar = ` wt · ${activeCount} worktree${activeCount === 1 ? "" : "s"}${archivedNote}${loadingNote} `;
  return (
    <box
      flexShrink={0}
      flexDirection="row"
      backgroundColor={theme.bgAlt}
      paddingLeft={1}
      paddingRight={1}
      height={1}
    >
      <box flexGrow={1} flexShrink={1} overflow="hidden" flexDirection="row">
        <text fg={theme.fgBright} attributes={1}>
          {titleBar}
        </text>
        {/* The animated wave (width = in-flight count) is the live
            "refreshing" signal; `loading...` wins during cold start. */}
        <box width={MAX_WAVE_WIDTH} flexShrink={0} overflow="hidden">
          <RefreshWave count={isLoading ? 0 : fetchingCount} fg={theme.fgDim} />
        </box>
        {remoteUnavailable ? (
          <text fg={theme.warn}>{` ⚠ ${config.remote?.label ?? "remote"} offline`}</text>
        ) : remoteVersionMismatch ? (
          <text fg={theme.warn}>{` ⚠ ${config.remote?.label ?? "remote"} version mismatch`}</text>
        ) : null}
      </box>
      {automationsConfigured && automationsPaused ? (
        // Inverse chip, not plain warn text: while paused the whole
        // automated leg of the escalation ladder is inert, which is a
        // different tier of fact than the CPU/mem telemetry it sits
        // next to — it must not blend into that cluster.
        <text fg={theme.bg} bg={theme.warn} attributes={1}>
          {" auto ⏸ "}
        </text>
      ) : automationsPending > 0 ? (
        <text fg={theme.fgDim}>{`auto ${automationsPending} queued  `}</text>
      ) : null}
      {automationsConfigured && automationsPaused ? <text>{"  "}</text> : null}
      <UsageBadge primary={primaryHarness} />
      <PrimaryHarnessBadge primary={primaryHarness} />
    </box>
  );
});
