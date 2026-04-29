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
import { TextAttributes } from "@opentui/core";
import { useQuery } from "@tanstack/react-query";

import { config } from "../../core/config.ts";
import { StatusKind } from "../../core/types.ts";
import { useGithub } from "../../state/hooks.ts";
import {
  aiSummaryQuery,
  wtDiffContextQuery,
  wtFirstCommitQuery,
} from "../../state/queries.ts";
import { resolveRows, type RowModule } from "../rows/index.ts";
import type { FetchLike, RowContext } from "../rows/types.ts";
import { NF } from "../icons.ts";
import { theme } from "../theme.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";

type Props = { row?: WorktreeRow };

const RESOLVED_ROWS: readonly RowModule[] = resolveRows(config.ui.rows);

/**
 * Row staleness glyph aggregated across a module's sources. "…" only
 * before any source has data; the spinner once anything is in flight
 * with cached data behind it; nothing when idle.
 */
function combinedGlyph(fs: readonly FetchLike[]): string {
  if (fs.length === 0) return "";
  const anyFetching = fs.some((f) => f.isFetching);
  if (!anyFetching) return "";
  const anyHasData = fs.some((f) => f.data !== undefined);
  return anyHasData ? NF.refresh : "…";
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
 * Title above the row stack: PR title if there is one (human-authored,
 * always wins), else the oldest commit subject on the branch (the dev's
 * "what is this" framing before a PR exists), else nothing.
 */
function TitleLine({ title }: { title: string | null }) {
  if (!title) return null;
  return (
    <box marginBottom={1}>
      <text fg={theme.fg} attributes={1} wrapMode="none" truncate>
        {title}
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
  isFetching,
  hasContext,
  blockedReason,
  error,
}: {
  summary: string | null;
  isFetching: boolean;
  hasContext: boolean;
  blockedReason: string | null;
  error: Error | null;
}) {
  // No AI config and no cached value → don't reserve space.
  if (!summary && !isFetching && !hasContext && !blockedReason && !error) return null;
  // Match the per-row staleness glyph in the rest of the details
  // pane: NF.refresh suffix whenever a fetch is in flight, regardless
  // of whether we already have a cached summary. Same rationale —
  // "something's happening, the visible text may be stale".
  const refreshSuffix = isFetching ? ` ${NF.refresh}` : "";
  let body: React.ReactNode;
  // Errors win over everything except an in-flight retry: while
  // re-fetching after a failure we'd rather show "generating…" than
  // the stale error. Once the retry settles, the error reappears (or
  // gets replaced by the new summary).
  if (error && !isFetching) {
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
  } else if (isFetching) {
    body = (
      <text fg={theme.fgDim} attributes={TextAttributes.ITALIC}>
        generating summary…{refreshSuffix}
      </text>
    );
  } else if (blockedReason) {
    body = <text fg={theme.fgDim}>{blockedReason}</text>;
  } else {
    body = <text fg={theme.fgDim}>no summary yet</text>;
  }
  return <box marginTop={1}>{body}</box>;
}

function DetailsBody({ row }: { row: WorktreeRow }) {
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

  // First-commit subject is cheap; pause it during destroys so we're
  // not racing the worktree's git state, but otherwise let it run.
  const firstCommit = useQuery({
    ...wtFirstCommitQuery(row.wt),
    enabled: !isBusy,
  });

  const diffCtx = useQuery({
    ...wtDiffContextQuery(row.wt),
    enabled: allowFetch,
  });

  const summary = useQuery({
    ...aiSummaryQuery(row.wt.slug, diffCtx.data ?? null),
    enabled: allowFetch && !!diffCtx.data,
  });

  const ctx: RowContext = { row, github };

  // Title hierarchy: LLM-generated wins (most context-aware and
  // up-to-date with the diff), then PR title, then the oldest commit's
  // subject as a non-AI fallback.
  const llmTitle = summary.data?.title ?? null;
  const prTitle =
    row.wt.branch && github.data?.prs ? github.data.prs[row.wt.branch]?.title : undefined;
  const title = llmTitle || prTitle || firstCommit.data || null;

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
      <TitleLine title={title} />
      {RESOLVED_ROWS.map((m) => (
        <RenderedRow key={m.id} module={m} ctx={ctx} />
      ))}
      <DescriptionBlock
        summary={summary.data?.description ?? null}
        isFetching={diffCtx.isFetching || summary.isFetching}
        hasContext={!!diffCtx.data}
        blockedReason={blockedReason}
        error={summary.error ?? diffCtx.error ?? null}
      />
    </box>
  );
}

export function Details({ row }: Props) {
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
  return <DetailsBody row={row} />;
}
