/**
 * Worktree details pane — driver only.
 *
 * Layout principles (relevant to row authors):
 *
 * - **Stable identity at top, dynamic activity at bottom.** The
 *   default order in `config.ui.rows` is tuned this way. Users can
 *   override but should follow the same churn-rate principle.
 *
 * - **Compact dense rows beat short stacked rows.** Related info
 *   collapses onto one line with `·`-separated segments — see
 *   `rows/git.tsx` and `rows/pr.tsx` for the established pattern.
 *
 * - **Staleness glyph and error display are handled here, not in row
 *   modules.** Modules declare their `sources`; this driver computes
 *   the trailing glyph and, once retries are exhausted, replaces the
 *   row body with the source's error message verbatim.
 *
 * - **Icons and colors follow `tui/badges.ts` and `tui/icons.ts`.**
 *   Same-concept-same-glyph between this pane and the row list is
 *   enforced via shared helpers.
 */
import { memo, useMemo } from "react";
import { TextAttributes } from "@opentui/core";
import { useQuery } from "@tanstack/react-query";

import { config } from "../../core/config.ts";
import { StatusKind } from "../../core/types.ts";
import { useGithub } from "../../state/hooks.ts";
import {
  aiSummaryQuery,
  wtDiffContextQuery,
} from "../../state/queries.ts";
import { resolveRows, type RowModule } from "../rows/index.ts";
import type { FetchLike, RowContext } from "../rows/types.ts";
import { NF } from "../icons.ts";
import { ELLIPSIS } from "../text.ts";
import { theme } from "../theme.ts";
import type { TitleSource, WorktreeRow } from "../hooks/useWorktreeRows.ts";

type Props = { row?: WorktreeRow; width: number };

const RESOLVED_ROWS: readonly RowModule[] = resolveRows(config.ui.rows);

/**
 * Row staleness glyph aggregated across a module's sources. "..." only
 * before any source has data; the spinner once anything is in flight
 * with cached data behind it; nothing when idle.
 */
function combinedGlyph(fs: readonly FetchLike[]): string {
  if (fs.length === 0) return "";
  const anyFetching = fs.some((f) => f.isFetching);
  if (!anyFetching) return "";
  const anyHasData = fs.some((f) => f.data !== undefined);
  return anyHasData ? NF.refresh : ELLIPSIS;
}

/**
 * First source whose retries are exhausted. Gating on `!isFetching`
 * suppresses the transient mid-retry error state — we only show the
 * banner once react-query has actually given up for now.
 */
function firstError(fs: readonly FetchLike[]): Error | null {
  for (const f of fs) {
    if (f.error && !f.isFetching) return f.error;
  }
  return null;
}

function Glyph({ text }: { text: string }) {
  if (!text) return null;
  return <text fg={theme.fgDim}> {text}</text>;
}

const LABEL_WIDTH = 8;
/** Reserved cells for the trailing staleness glyph slot (` ` + 1-cell glyph). Matches `<Glyph>`'s leading space. */
const GLYPH_SLOT_WIDTH = 2;
/** Border (1 left + 1 right) + content padding (1 each side). */
const PANE_CHROME_WIDTH = 4;

/** Compute the row-value cell budget from the pane's outer width. */
function valueWidthFor(paneWidth: number): number {
  return Math.max(0, paneWidth - PANE_CHROME_WIDTH - LABEL_WIDTH - GLYPH_SLOT_WIDTH);
}

/**
 * One detail row: fixed-width label, value sized to content but
 * shrinkable with ellipsis when the pane is too narrow. Optional
 * trailing content (e.g. a staleness glyph) sits directly after the
 * value — not flush-right — and never shrinks.
 */
function Row({
  label,
  children,
  trailing,
}: {
  label: string;
  children: React.ReactNode;
  trailing?: React.ReactNode;
}) {
  return (
    <box flexDirection="row">
      <box width={LABEL_WIDTH} flexShrink={0}>
        <text fg={theme.fgDim}>{label}</text>
      </box>
      <box flexShrink={1} overflow="hidden">
        {children}
      </box>
      {trailing ? <box flexShrink={0}>{trailing}</box> : null}
    </box>
  );
}

function RenderedRow({ module: m, ctx }: { module: RowModule; ctx: RowContext }) {
  if (m.visible && !m.visible(ctx)) return null;
  const sources = m.sources ? m.sources(ctx) : [];
  const err = firstError(sources);
  const glyph = combinedGlyph(sources);
  return (
    <Row
      label={m.label}
      trailing={glyph ? <Glyph text={glyph} /> : undefined}
    >
      {err ? (
        <text fg={theme.err} wrapMode="none" truncate>
          {err.message}
        </text>
      ) : (
        m.render(ctx)
      )}
    </Row>
  );
}

/**
 * Title above the row stack. Bold title followed by a muted `(source)`
 * tag so it's obvious where the text came from — useful for spotting
 * stale PR titles vs. fresh LLM-generated ones at a glance. Always
 * renders: `useWorktreeRows` guarantees a slug-derived fallback, so
 * the line count stays stable as the better sources fill in.
 */
function TitleLine({
  title,
  source,
}: {
  title: string;
  source: TitleSource;
}) {
  return (
    <box marginBottom={1}>
      <text wrapMode="none" truncate>
        <span fg={theme.fg} attributes={1}>{title}</span>
        <span fg={theme.fgDim}>{` (${source})`}</span>
      </text>
    </box>
  );
}

/**
 * Multi-line AI summary below the rows. Renders muted text, falls back
 * to a placeholder while the first generation is in flight, and stays
 * silent on errors / when the row is dirty-but-uncached (avoid noise).
 * `null` summary means "AI not configured" and the section is omitted
 * entirely.
 */
function DescriptionBlock({
  summary,
  isLlmRunning,
  hasContext,
  blockedReason,
  error,
}: {
  summary: string | null;
  /**
   * True only while the LM Studio call itself is in flight — *not* for
   * the cheap diff-context revalidation. Drives the refresh glyph and
   * the "generating summary…" placeholder. Intentionally narrower than
   * the per-row staleness glyph elsewhere: a hash-stable cache check
   * shouldn't make this row look like it's regenerating when it isn't.
   */
  isLlmRunning: boolean;
  hasContext: boolean;
  blockedReason: string | null;
  error: Error | null;
}) {
  // No AI config and no cached value → don't reserve space.
  if (!summary && !isLlmRunning && !hasContext && !blockedReason && !error) return null;
  const refreshSuffix = isLlmRunning ? ` ${NF.refresh}` : "";
  let body: React.ReactNode;
  // Errors win over everything except an in-flight retry: while
  // re-fetching after a failure we'd rather show "generating…" than
  // the stale error. Once the retry settles, the error reappears (or
  // gets replaced by the new summary).
  if (error && !isLlmRunning) {
    body = (
      <text fg={theme.err} wrapMode="word">
        {error.message}
      </text>
    );
  } else if (summary) {
    body = (
      <text fg={theme.fgDim} attributes={TextAttributes.ITALIC} wrapMode="word">
        {summary}
        {refreshSuffix}
      </text>
    );
  } else if (isLlmRunning) {
    body = (
      <text fg={theme.fgDim} attributes={TextAttributes.ITALIC}>
        generating summary{ELLIPSIS}{refreshSuffix}
      </text>
    );
  } else if (blockedReason) {
    body = <text fg={theme.fgDim}>{blockedReason}</text>;
  } else {
    body = <text fg={theme.fgDim}>no summary yet</text>;
  }
  return <box marginTop={1}>{body}</box>;
}

const DetailsBody = memo(function DetailsBody({ row, width }: { row: WorktreeRow; width: number }) {
  // Subscribe to the combined GitHub fetch so per-row indicators
  // reflect its fetch state. Observers dedupe by key — this doesn't
  // trigger an extra fetch, it joins the existing observer in
  // `useWorktreeRows`.
  const github = useGithub();

  const isBusy = row.status.kind === StatusKind.Busy;
  // The diff context is `base..HEAD` only — uncommitted work is never
  // included, so a dirty tree doesn't change what the AI would see.
  // Only pause for busy worktrees, where racing the destroy is unsafe.
  const aiEnabled = !!config.ai;
  const allowFetch = aiEnabled && !isBusy;

  // Diff context + summary observers are duplicated with `useWorktreeRows`
  // (cache-shared, not refetched) so this pane has direct access to the
  // *description* and the per-fetch state for the spinner / error
  // gating, neither of which is exposed on `WorktreeRow`. The resolved
  // title itself comes pre-computed from the row.
  const diffCtx = useQuery({
    ...wtDiffContextQuery(row.wt),
    enabled: allowFetch,
  });

  const summary = useQuery({
    ...aiSummaryQuery(row.wt.slug, diffCtx.data ?? null),
    enabled: allowFetch && !!diffCtx.data,
  });

  const valueWidth = valueWidthFor(width);
  const ctx: RowContext = useMemo(
    () => ({ row, github, valueWidth }),
    [row, github, valueWidth],
  );

  // Pick a single user-facing reason when we're suppressing AI work.
  const blockedReason: string | null = !aiEnabled
    ? null
    : isBusy
      ? "summary paused while worktree is busy"
      : null;

  return (
    <box
      flexGrow={1}
      flexShrink={1}
      overflow="hidden"
      border
      borderStyle="single"
      borderColor={theme.border}
      title={` ${row.wt.slug} `}
      titleAlignment="left"
      padding={1}
      flexDirection="column"
    >
      <TitleLine title={row.title} source={row.titleSource} />
      {RESOLVED_ROWS.map((m) => (
        <RenderedRow key={m.id} module={m} ctx={ctx} />
      ))}
      <DescriptionBlock
        summary={summary.data?.description ?? null}
        isLlmRunning={summary.isFetching}
        hasContext={!!diffCtx.data}
        blockedReason={blockedReason}
        error={summary.error ?? diffCtx.error ?? null}
      />
    </box>
  );
});

export function Details({ row, width }: Props) {
  if (!row) {
    return (
      <box
        flexGrow={1}
        border
        borderStyle="single"
        borderColor={theme.border}
        title=" details "
        titleAlignment="left"
        padding={1}
      >
        <text fg={theme.fgDim}>No worktree selected.</text>
      </box>
    );
  }
  return <DetailsBody row={row} width={width} />;
}
