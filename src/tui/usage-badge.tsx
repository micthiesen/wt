/**
 * Right-aligned title-bar usage slot. Two clusters — 5h window and
 * rolling 7d window — each pairing the percentage with the time
 * remaining until that window resets. The 30-second ticker keeps the
 * remaining-time labels drifting forward between refetches.
 *
 * Freshness is gated per-window on `resetsAt`, not on the source file's
 * mtime: a window's percentage stays valid (usage only climbs within a
 * window) until the moment it resets, at which point that cluster drops
 * off. This matters for codex, whose rollout only updates when it writes
 * a turn — an hour-idle codex still has a valid 5h reading — while claude
 * gets the same correct behavior (a genuinely-reset window disappears).
 */
import { useEffect, useState } from "react";
import { useQuery } from "@tanstack/react-query";

import type { ClaudeUsage } from "../core/claude-usage.ts";
import type { CodexUsage } from "../core/harness/codex-usage.ts";
import { getHarness, type HarnessId } from "../core/harness/index.ts";
import {
  claudeUsageQuery,
  codexUsageQuery,
  opencodeCostQuery,
} from "../state/index.ts";

import { theme } from "./theme.ts";

/**
 * Top-right harness selector indicator: just the primary harness's
 * glyph, brand-colored. Rendered as the rightmost element of the title
 * bar so it anchors to the edge and stays put as the usage figures to
 * its left change width — no label or TAB hint (the cycle key is muscle
 * memory). Tabbing is wired in the main keypress handler.
 */
export function PrimaryHarnessBadge({ primary }: { primary: HarnessId }) {
  const harness = getHarness(primary);
  return (
    <box flexShrink={0} flexDirection="row">
      <text fg={harness.color}>{harness.glyph}</text>
    </box>
  );
}

/**
 * Top-right usage slot, following the Shift+TAB-selected primary harness:
 *   - claude / codex → rate-limit windows as `5h X% / 7d Y%`
 *   - opencode       → spend over the same windows as `5h $X / 7d $Y`
 *     (it has no rate-limit window — it bills per token)
 * Each source is gated to its primary so we don't scan rollouts / hit
 * the opencode DB when that harness isn't selected.
 */
export function UsageBadge({ primary }: { primary: HarnessId }) {
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNowMs(Date.now()), 30_000);
    return () => clearInterval(id);
  }, []);
  const claude = useQuery({
    ...claudeUsageQuery(),
    enabled: primary === "claude",
  });
  const codex = useQuery({ ...codexUsageQuery(), enabled: primary === "codex" });
  const opencode = useQuery({
    ...opencodeCostQuery(),
    enabled: primary === "opencode",
  });

  if (primary === "opencode") {
    const cost = opencode.data;
    if (!cost) return null;
    return (
      <box flexShrink={0} flexDirection="row">
        <text>
          <span fg={theme.fg}>{`5h ${formatCost(cost.fiveHour)}`}</span>
          <span fg={theme.fgDim}>{" · "}</span>
          <span fg={theme.fg}>{`7d ${formatCost(cost.sevenDay)}`}</span>
          <span fg={theme.fgDim}>{" · "}</span>
        </text>
      </box>
    );
  }

  const formatted = formatPctUsage(
    primary === "claude" ? claude.data : codex.data,
    nowMs,
  );
  if (!formatted) return null;
  const { fiveHour: five, sevenDay: seven } = formatted;
  return (
    <box flexShrink={0} flexDirection="row">
      <text>
        <span fg={pctColor(five.pct)}>{`5h ${five.pct}%`}</span>
        {five.remaining ? (
          <span fg={theme.fgDim}>{` (${five.remaining})`}</span>
        ) : null}
        <span fg={theme.fgDim}>{" · "}</span>
        <span fg={pctColor(seven.pct)}>{`7d ${seven.pct}%`}</span>
        {seven.remaining ? (
          <span fg={theme.fgDim}>{` (${seven.remaining})`}</span>
        ) : null}
        <span fg={theme.fgDim}>{" · "}</span>
      </text>
    </box>
  );
}

/** Compact USD: cents-precise under $100, whole dollars above. */
function formatCost(n: number): string {
  return n >= 100 ? `$${Math.round(n)}` : `$${n.toFixed(2)}`;
}

/**
 * Match statusline.sh's coloring: cool/dim under 60%, warm 60-80%,
 * hot at 80%+. Only the percentage itself is tinted — the surrounding
 * "5h ..." / "(time)" framing stays dim so the colored numbers pop.
 */
function pctColor(pct: number): string {
  if (pct >= 80) return theme.err;
  if (pct >= 60) return theme.warn;
  return theme.fg;
}

type PctWindow = { pct: number; remaining: string | null };
type FormattedUsage = {
  fiveHour: PctWindow;
  sevenDay: PctWindow;
};

/**
 * Format one window. Past its `resetsAt` the cached percentage describes
 * the *previous* window; the current one starts fresh at ~0 (no usage
 * accrues while the source is idle), so we report `0%` with no countdown
 * rather than the stale figure — or a blank, which is the gap a just-
 * reset window briefly hits before the source rewrites its `resetsAt`.
 * The real value (and countdown) returns on the next refresh.
 */
function pctWindow(
  p: { utilization: number; resetsAt: string | null },
  nowMs: number,
): PctWindow {
  if (p.resetsAt) {
    const t = Date.parse(p.resetsAt);
    if (!Number.isNaN(t) && nowMs >= t) return { pct: 0, remaining: null };
  }
  return {
    pct: Math.round(p.utilization),
    remaining: formatRemaining(p.resetsAt, nowMs),
  };
}

function formatPctUsage(
  usage: ClaudeUsage | CodexUsage | null | undefined,
  nowMs: number,
): FormattedUsage | null {
  if (!usage) return null;
  return {
    fiveHour: pctWindow(usage.fiveHour, nowMs),
    sevenDay: pctWindow(usage.sevenDay, nowMs),
  };
}

/**
 * Format a duration as the two coarsest non-zero units. Picks d+h when
 * the duration spans days, h+m otherwise. Drops the smaller unit when
 * it would render as 0 — `2h0m` becomes `2h`. Returns null on missing
 * or unparseable input.
 */
function formatRemaining(iso: string | null, nowMs: number): string | null {
  if (!iso) return null;
  const target = Date.parse(iso);
  if (Number.isNaN(target)) return null;
  const ms = Math.max(0, target - nowMs);
  const d = Math.floor(ms / 86_400_000);
  const h = Math.floor((ms % 86_400_000) / 3_600_000);
  const m = Math.floor((ms % 3_600_000) / 60_000);
  if (d > 0) return h > 0 ? `${d}d${h}h` : `${d}d`;
  if (h > 0) return m > 0 ? `${h}h${m}m` : `${h}h`;
  return `${m}m`;
}
