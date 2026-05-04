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
import { memo, useEffect, useMemo } from "react";
import { TextAttributes } from "@opentui/core";
import { useQuery, useQueryClient } from "@tanstack/react-query";

import { config } from "../../core/config.ts";
import { StatusKind } from "../../core/types.ts";
import { useGithub } from "../../state/hooks.ts";
import { qk } from "../../state/keys.ts";
import {
  aiSummaryQuery,
  wtDiffContextQuery,
} from "../../state/queries.ts";
import { resolveRows, type RowModule } from "../rows/index.ts";
import type { FetchLike, RowContext } from "../rows/types.ts";
import { ELLIPSIS } from "../text.ts";
import { Spinner, useBouncingBall } from "../spinner.tsx";
import { theme } from "../theme.ts";
import type { TitleSource, WorktreeRow } from "../hooks/useWorktreeRows.ts";

type Props = { row?: WorktreeRow; width: number };

const RESOLVED_ROWS: readonly RowModule[] = resolveRows(config.ui.rows);

type GlyphKind = "spinner" | "ellipsis" | null;

/**
 * Row staleness indicator aggregated across a module's sources. Ellipsis
 * before any source has data (cold load); the animated spinner once
 * anything is in flight with cached data behind it; nothing when idle.
 */
function combinedGlyph(fs: readonly FetchLike[]): GlyphKind {
  if (fs.length === 0) return null;
  const anyFetching = fs.some((f) => f.isFetching);
  if (!anyFetching) return null;
  const anyHasData = fs.some((f) => f.data !== undefined);
  return anyHasData ? "spinner" : "ellipsis";
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

function Glyph({ kind }: { kind: GlyphKind }) {
  if (kind === null) return null;
  if (kind === "spinner") return <Spinner fg={theme.fgDim} />;
  return <text fg={theme.fgDim}>{ELLIPSIS}</text>;
}

/**
 * Italic summary line with a trailing spinner. Sub-component (rather
 * than embedding the spinner via `<span>` inside `<text>`) so the
 * spinner frame ends up as plain text content, which the reconciler
 * updates reliably.
 */
function SummaryWithSpinner({ summary }: { summary: string }) {
  const frame = useBouncingBall();
  return (
    <text fg={theme.fgDim} attributes={TextAttributes.ITALIC} wrapMode="word">
      {summary} {frame}
    </text>
  );
}

function GeneratingLine() {
  const frame = useBouncingBall();
  return (
    <text fg={theme.fgDim} attributes={TextAttributes.ITALIC}>
      Generating summary{ELLIPSIS} {frame}
    </text>
  );
}

/**
 * Right-aligned label column width: longest configured row label plus a
 * one-cell gap before the value column. Computed at module init from
 * `RESOLVED_ROWS` so reconfiguring `ui.rows` reclaims unused cells.
 */
const LABEL_WIDTH =
  RESOLVED_ROWS.reduce((m, r) => Math.max(m, r.label.length), 0) + 1;
/** Reserved cells for the trailing staleness glyph slot (1-cell `paddingLeft` + 2-cell spinner). */
const GLYPH_SLOT_WIDTH = 3;
/** Border (1 left + 1 right) + content padding (1 each side). */
const PANE_CHROME_WIDTH = 4;

/** Compute the row-value cell budget from the pane's outer width. */
function valueWidthFor(paneWidth: number): number {
  return Math.max(0, paneWidth - PANE_CHROME_WIDTH - LABEL_WIDTH - GLYPH_SLOT_WIDTH);
}

/**
 * One detail row: fixed-width right-aligned label, value sized to
 * content but shrinkable with ellipsis when the pane is too narrow.
 * Optional trailing content (e.g. a staleness glyph) sits directly
 * after the value — not flush-right — and never shrinks.
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
      <box
        width={LABEL_WIDTH}
        flexShrink={0}
        flexDirection="row"
        justifyContent="flex-end"
        paddingRight={1}
      >
        <text fg={theme.fgDim}>{label}</text>
      </box>
      <box flexShrink={1} overflow="hidden">
        {children}
      </box>
      {trailing ? <box flexShrink={0} paddingLeft={1}>{trailing}</box> : null}
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
      trailing={glyph ? <Glyph kind={glyph} /> : undefined}
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
 * Most recent top-level review message, formatted as `@author: body`.
 * Sits between the row stack and the AI description so it's adjacent to
 * the review badge above and the LLM-generated context below. Renders
 * nothing when there's no review or no body — the reviewer hit Approve
 * without typing anything.
 */
function ReviewBlock({
  review,
}: {
  review: { author: string; body: string } | null;
}) {
  if (!review) return null;
  return (
    <box marginTop={1}>
      <text fg={theme.fg} wrapMode="word">
        <span attributes={TextAttributes.BOLD}>@{review.author}</span>
        {`: ${review.body}`}
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
    body = isLlmRunning ? (
      <SummaryWithSpinner summary={summary} />
    ) : (
      <text fg={theme.fgDim} attributes={TextAttributes.ITALIC} wrapMode="word">
        {summary}
      </text>
    );
  } else if (isLlmRunning) {
    body = <GeneratingLine />;
  } else if (blockedReason) {
    body = <text fg={theme.fgDim}>{blockedReason}</text>;
  } else {
    body = <text fg={theme.fgDim}>No summary yet</text>;
  }
  return <box marginTop={1}>{body}</box>;
}

const DetailsBody = memo(function DetailsBody({ row, width }: { row: WorktreeRow; width: number }) {
  const qc = useQueryClient();
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
  //
  // Effective base must match the one `useWorktreeRows` used or the two
  // observers fight: each writes to a different per-(slug, base) cache
  // entry, the AI memo lookup hashes differently, and the LM Studio
  // call would re-run every time the details pane mounts for a stacked
  // worktree. Only the commit-ancestry signal feeds the diff base —
  // PR-base alone (via: "pr") doesn't, since it can be stale or
  // unrebased. Mirrors `effectiveBaseFor` in `useWorktreeRows`.
  const stackedBase =
    row.stackedOn?.via === "commits" ? row.stackedOn.branch : null;
  const diffCtx = useQuery({
    ...wtDiffContextQuery(row.wt, stackedBase),
    enabled: allowFetch,
  });

  const summary = useQuery({
    ...aiSummaryQuery(qc, row.wt.slug, diffCtx.data ?? null),
    enabled: allowFetch && !!diffCtx.data,
  });

  // Slug-keyed cache: the queryFn doesn't re-run on diff drift unless
  // we invalidate. Mirror the effect in `useWorktreeRows` so the
  // details pane refreshes too — both observers share the same cache
  // entry, but only the one with the freshest reactive state will
  // notice the mismatch. `useWorktreeRows` covers list-only renders;
  // this covers the case where the details pane is the only mount.
  const dataHash = summary.data?.hash;
  const ctxHash = diffCtx.data?.hash;
  useEffect(() => {
    if (dataHash && ctxHash && dataHash !== ctxHash) {
      void qc.invalidateQueries({ queryKey: qk.aiSummary(row.wt.slug) });
    }
  }, [dataHash, ctxHash, row.wt.slug, qc]);

  const valueWidth = valueWidthFor(width);
  const ctx: RowContext = useMemo(
    () => ({ row, github, valueWidth }),
    [row, github, valueWidth],
  );

  // Pick a single user-facing reason when we're suppressing AI work.
  const blockedReason: string | null = !aiEnabled
    ? null
    : isBusy
      ? "Summary paused while worktree is busy"
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
      <ReviewBlock review={row.pr?.latestReview ?? null} />
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
