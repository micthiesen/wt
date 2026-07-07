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
import { memo, type RefObject } from "react";
import { TextAttributes } from "@opentui/core";
import type { ScrollBoxRenderable } from "@opentui/core";
import { keepPreviousData, useQuery } from "@tanstack/react-query";

import { config } from "../../core/config.ts";
import type { ReviewRequestPr } from "../../core/github.ts";
import { StatusKind, type PrComment } from "../../core/types.ts";
import type { HarnessId } from "../../core/harness/index.ts";
import type { DerivedState } from "../../core/claude-status.ts";
import { useGithub } from "../../state/hooks.ts";
import {
  aiSummaryQuery,
  wtDiffContextQuery,
} from "../../state/queries.ts";
import { resolveRows, type RowModule } from "../rows/index.ts";
import type { FetchLike, RowContext } from "../rows/types.ts";
import { useScrollbarNoFlash } from "../hooks/useScrollbarNoFlash.ts";
import { ageMsToText, ELLIPSIS } from "../text.ts";
import { Spinner, useBouncingBall } from "../spinner.tsx";
import { NF } from "../icons.ts";
import { checkBadge, reviewBadge, statusBadge } from "../badges.ts";
import { BadgeCluster } from "../badge-cluster.tsx";
import { laneColor, theme } from "../theme.ts";
import type { TitleSource, WorktreeRow } from "../hooks/useWorktreeRows.ts";
import type { StackManifest } from "../../core/wtstate.ts";
import {
  layoutStack,
  STACK_CONNECTOR,
  stackOrdinalLabel,
  type SpinePos,
} from "../../core/stack-layout.ts";

/**
 * What the detail pane shows when a FOLDED section header is the cursor: the
 * stack/section overview. Built by `app.tsx` from the folded section item +
 * the live manifest, so this pane stays free of state reads.
 */
export type SectionMember = {
  /** Same label the list row shows (`rowLabel`), so the folded summary
   *  and the expanded rows read identically. */
  label: string;
  /** The live list row — status/archived plus everything the shared
   *  badge cluster reads (pr, mq, deploy). */
  row: WorktreeRow;
  /** Badge-cluster inputs the list pane computes per slug (action
   *  glyph, harness session glyph + tint), passed through so the
   *  folded summary shows the identical cluster. */
  actionRunning: boolean;
  activeHarnessId: HarnessId | undefined;
  sessionState: DerivedState | undefined;
};

export type SectionDetail = {
  /** Stable section identity — keys the body so an AI-title label change
   *  doesn't remount the pane under a stationary cursor. */
  sectionKey: string;
  isStack: boolean;
  label: string;
  manifest: StackManifest | null;
  members: SectionMember[];
};

type Props = {
  row?: WorktreeRow;
  reviewRequest?: ReviewRequestPr;
  /** Set when a folded section header is selected — shows the stack summary. */
  section?: SectionDetail;
  width: number;
  /**
   * Ref to the inner scrollbox of whichever body is mounted, so the
   * app's global key handler can page it on PageUp/PageDown. Only one
   * body mounts at a time, so a single ref covers both the worktree and
   * review-request panes; switching rows remounts the box and resets
   * scroll to the top.
   */
  scrollRef?: RefObject<ScrollBoxRenderable | null>;
};

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
  labelWidth = LABEL_WIDTH,
}: {
  label: string;
  children: React.ReactNode;
  trailing?: React.ReactNode;
  labelWidth?: number;
}) {
  return (
    <box flexDirection="row">
      <box
        width={labelWidth}
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
 * One comment: a `@author · 2h ago` meta line, then the body on the
 * lines below (bodies can be long / multi-line, so they get their own
 * line rather than flowing after a colon). A malformed timestamp just
 * drops the age suffix.
 */
function CommentLine({ comment, first }: { comment: PrComment; first: boolean }) {
  const ts = Date.parse(comment.createdAt);
  const age = Number.isFinite(ts) ? ` · ${ageMsToText(Date.now() - ts)} ago` : "";
  return (
    <box marginTop={first ? 0 : 1}>
      <text fg={theme.fg} wrapMode="word">
        <span attributes={TextAttributes.BOLD}>@{comment.author}</span>
        <span fg={theme.fgDim}>{age}</span>
        {`\n${comment.body}`}
      </text>
    </box>
  );
}

/**
 * The PR's human conversation, newest-first: issue comments + review
 * bodies (bots already filtered out upstream). A trailing dim line
 * reports unresolved review threads, whose bodies we deliberately don't
 * inline. Renders nothing when there's neither a comment nor an open
 * thread. Sits at the bottom of the pane, below the AI description.
 */
function CommentsBlock({
  comments,
  unresolvedThreads,
}: {
  comments: readonly PrComment[];
  unresolvedThreads: number;
}) {
  if (comments.length === 0 && unresolvedThreads === 0) return null;
  return (
    <box marginTop={1} flexDirection="column">
      {comments.map((c, i) => (
        <CommentLine key={`${c.author}-${c.createdAt}-${i}`} comment={c} first={i === 0} />
      ))}
      {unresolvedThreads > 0 ? (
        <box marginTop={comments.length > 0 ? 1 : 0}>
          <text fg={theme.fgDim}>
            {`+${unresolvedThreads} unresolved ${unresolvedThreads === 1 ? "thread" : "threads"}`}
          </text>
        </box>
      ) : null}
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

const DetailsBody = memo(function DetailsBody({
  row,
  width,
  scrollRef,
}: {
  row: WorktreeRow;
  width: number;
  scrollRef?: RefObject<ScrollBoxRenderable | null>;
}) {
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
  // The diff base is resolved once in `useWorktreeRows` and exposed via
  // `row.stackedOn.diffBase`. Reading it from there (rather than
  // re-deriving) keeps the two observers' query keys identical, which
  // is what lets the cache hit cross-pane and avoids re-running LM
  // Studio every time the details pane mounts for a stacked worktree.
  const sbRef = useScrollbarNoFlash(scrollRef);
  const effectiveBase = row.stackedOn?.diffBase ?? null;
  const diffCtx = useQuery({
    ...wtDiffContextQuery(row.wt, effectiveBase),
    enabled: allowFetch,
  });

  // Hash-keyed AI summary: when the diff hash changes, the queryKey
  // changes; `keepPreviousData` keeps the prior summary on screen
  // while the new fetch runs. The mismatch-detect effect that lived
  // here in the slug-keyed era is gone — the cache key swap *is* the
  // trigger now.
  const summary = useQuery({
    ...aiSummaryQuery(row.wt.slug, diffCtx.data ?? null),
    enabled: allowFetch && !!diffCtx.data,
    placeholderData: keepPreviousData,
  });

  const valueWidth = valueWidthFor(width);
  // No useMemo: `github` is a UseQueryResult wrapper with a fresh identity
  // every render, so a memo keyed on it would never hold anyway — build
  // the ctx inline and keep the cost honest. Rows re-render with the pane
  // regardless (none are React.memo'd on ctx identity).
  const ctx: RowContext = { row, github, valueWidth };

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
      <scrollbox
        ref={sbRef}
        scrollY
        flexGrow={1}
        minHeight={0}
        contentOptions={{ flexDirection: "column" }}
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
        <CommentsBlock
          comments={row.pr?.comments ?? []}
          unresolvedThreads={row.pr?.unresolvedThreads ?? 0}
        />
      </scrollbox>
    </box>
  );
});

/**
 * Right-aligned label width for the review-request body. Independent of
 * the configured `RESOLVED_ROWS`, so it sizes to its *own* labels (+2 gap)
 * rather than borrowing the narrower shared `LABEL_WIDTH`, which clipped
 * longer labels like "branch"/"review".
 */
const RR_LABEL_WIDTH =
  ["state", "branch", "author", "diff", "status", "age"].reduce(
    (m, l) => Math.max(m, l.length),
    0,
  ) + 2;

/** One review-request row: shared wide label column. */
function RRRow({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <Row label={label} labelWidth={RR_LABEL_WIDTH}>
      {children}
    </Row>
  );
}

/** Map GitHub's `reviewDecision` to a glyph + color + human label. */
function reviewDecisionBadge(
  d: ReviewRequestPr["reviewDecision"],
): { glyph: string; fg: string; label: string } | null {
  switch (d) {
    case "APPROVED": {
      const b = reviewBadge("approved");
      return b ? { ...b, label: "approved" } : null;
    }
    case "CHANGES_REQUESTED": {
      const b = reviewBadge("changes_requested");
      return b ? { ...b, label: "changes requested" } : null;
    }
    case "REVIEW_REQUIRED": {
      const b = reviewBadge("pending");
      return b ? { ...b, label: "review required" } : null;
    }
    default:
      return null;
  }
}

/**
 * Details body for a review-request PR. Not a worktree — no local
 * checkout, no per-slug sources, no AI summary pipeline — so it renders
 * straight from the PR search payload. Mirrors the worktree details
 * aesthetic: right-aligned labels, glyph-led values, and dense
 * `·`-separated lines (diff size, CI + review, ages) rather than one
 * stacked row per field. `p` opens it on GitHub from the parent; this
 * pane is read-only.
 */
function ReviewRequestBody({
  pr,
  width: _width,
  scrollRef,
}: {
  pr: ReviewRequestPr;
  width: number;
  scrollRef?: RefObject<ScrollBoxRenderable | null>;
}) {
  const created = pr.createdAt ? Date.parse(pr.createdAt) : NaN;
  const updated = pr.updatedAt ? Date.parse(pr.updatedAt) : NaN;
  const openedText = Number.isFinite(created)
    ? `opened ${ageMsToText(Date.now() - created)} ago`
    : null;
  const updatedText =
    Number.isFinite(updated) && Number.isFinite(created) && updated !== created
      ? `updated ${ageMsToText(Date.now() - updated)} ago`
      : null;

  const sbRef = useScrollbarNoFlash(scrollRef);
  const check = checkBadge(pr.checks);
  const review = reviewDecisionBadge(pr.reviewDecision);
  const hasDiff = pr.additions > 0 || pr.deletions > 0 || pr.changedFiles > 0;
  return (
    <box
      flexGrow={1}
      flexShrink={1}
      overflow="hidden"
      border
      borderStyle="single"
      borderColor={theme.border}
      title={` ${pr.repoNameWithOwner}#${pr.number} `}
      titleAlignment="left"
      padding={1}
      flexDirection="column"
    >
      <scrollbox
        ref={sbRef}
        scrollY
        flexGrow={1}
        minHeight={0}
        contentOptions={{ flexDirection: "column" }}
      >
      <box marginBottom={1}>
        <text wrapMode="word">
          <span fg={theme.fg} attributes={TextAttributes.BOLD}>{pr.title}</span>
        </text>
      </box>
      <RRRow label="state">
        <text fg={pr.isDraft ? theme.fgDim : theme.accentAlt} wrapMode="none">
          {`${pr.isDraft ? NF.prDraft : NF.prOpen}  ${pr.isDraft ? "draft" : "ready"}`}
        </text>
      </RRRow>
      {pr.headRefName ? (
        <RRRow label="branch">
          <text fg={theme.fg} wrapMode="none" truncate>
            {pr.headRefName}
          </text>
        </RRRow>
      ) : null}
      {pr.author ? (
        <RRRow label="author">
          <text fg={theme.fg} wrapMode="none" truncate>
            {`@${pr.author}`}
          </text>
        </RRRow>
      ) : null}
      {hasDiff ? (
        <RRRow label="diff">
          <text wrapMode="none" truncate>
            <span fg={theme.warn}>{`+${pr.additions}`}</span>
            <span> </span>
            <span fg={theme.err}>{`−${pr.deletions}`}</span>
            {pr.changedFiles > 0 ? (
              <span fg={theme.fgDim}>
                {` · ${pr.changedFiles} ${pr.changedFiles === 1 ? "file" : "files"}`}
              </span>
            ) : null}
            {pr.commentCount > 0 ? (
              <span fg={theme.fgDim}>{` · ${NF.comment}  ${pr.commentCount}`}</span>
            ) : null}
          </text>
        </RRRow>
      ) : null}
      {check || review ? (
        <RRRow label="status">
          <text wrapMode="none">
            {check ? (
              <span fg={check.fg}>{`${check.glyph}  ${pr.checks === "pass" ? "passing" : pr.checks === "fail" ? "failing" : "pending"}`}</span>
            ) : null}
            {check && review ? <span fg={theme.fgDim}>{" · "}</span> : null}
            {review ? (
              <span fg={review.fg}>{`${review.glyph}  ${review.label}`}</span>
            ) : null}
          </text>
        </RRRow>
      ) : null}
      {openedText ? (
        <RRRow label="age">
          <text fg={theme.fgDim} wrapMode="none" truncate>
            {updatedText ? `${openedText} · ${updatedText}` : openedText}
          </text>
        </RRRow>
      ) : null}
      <box marginTop={1}>
        <text fg={theme.fgDim} wrapMode="none" truncate>
          {pr.url}
        </text>
      </box>
      </scrollbox>
    </box>
  );
}

/** Status glyph + color for one slice in the folded-stack summary. */
function sliceGlyph(status: StackManifest["slices"][number]["status"]): {
  t: string;
  fg: string;
} {
  if (status === "merged") return { t: "✓", fg: theme.ok };
  if (status === "open") return { t: "○", fg: theme.warn };
  return { t: "·", fg: theme.fgDim };
}

/** The stack chain (spine · ordinal · status · title · badges), like
 *  `wt stack status`. Rows come from `layoutStack` so the lane order,
 *  connector glyphs, and ordinal labels match the expanded list gutter
 *  exactly; the right side renders the shared list-pane badge cluster
 *  for slices with a live worktree (matched by branch), falling back
 *  to the dim PR number for slices without one (planned, or merged +
 *  cleaned). */
function StackChain({
  manifest,
  members,
}: {
  manifest: StackManifest;
  members: SectionMember[];
}) {
  const memberByBranch = new Map(members.map((m) => [m.row.wt.branch, m]));
  const count = (s: StackManifest["slices"][number]["status"]) =>
    manifest.slices.filter((x) => x.status === s).length;
  const nodes = layoutStack(manifest).nodes;
  // layoutStack degrades gracefully on a malformed manifest (cycle /
  // dangling parent) by dropping the affected slices; append those flat
  // so the summary still lists every slice.
  const laidOut = new Set(nodes.map((n) => n.slice.id));
  const rows: { slice: StackManifest["slices"][number]; pos: SpinePos; lane: number }[] = [
    ...nodes.map((n) => ({ slice: n.slice, pos: n.pos, lane: n.lane })),
    ...manifest.slices
      .filter((s) => !laidOut.has(s.id))
      .sort((a, b) => a.ordinal - b.ordinal)
      .map((s) => ({ slice: s, pos: "single" as SpinePos, lane: 0 })),
  ];
  return (
    <>
      <text fg={theme.fgDim} wrapMode="none" truncate>
        {count("merged")} merged · {count("open")} open · {count("planned")} planned
      </text>
      <box height={1} flexShrink={0} />
      {rows.map(({ slice: s, pos, lane }) => {
        const g = sliceGlyph(s.status);
        const member = memberByBranch.get(s.branch);
        return (
          <box key={s.id} flexDirection="row">
            {/* The lead glyphs must never shrink — when the row overflows,
                yoga squeezing these texts garbles the spine; the title is
                the only flexible (truncating) segment. */}
            <box flexShrink={0} flexDirection="row">
              <text fg={laneColor(lane)} wrapMode="none">{STACK_CONNECTOR[pos]}</text>
              <text fg={theme.fgDim} wrapMode="none">{`${stackOrdinalLabel(s.ordinal)} `}</text>
              <text fg={g.fg} wrapMode="none">{`${g.t} `}</text>
            </box>
            {/* Flex-grow + overflow-hidden gives the title a bounded width so
                `truncate` ellipsises a long slice title instead of overflowing
                the row and garbling the pane. */}
            <box flexGrow={1} flexShrink={1} overflow="hidden">
              <text fg={theme.fg} wrapMode="none" truncate>{s.title}</text>
            </box>
            {member ? (
              <BadgeCluster
                row={member.row}
                actionRunning={member.actionRunning}
                activeHarnessId={member.activeHarnessId}
                sessionState={member.sessionState}
              />
            ) : s.pr ? (
              <box flexShrink={0}>
                <text fg={theme.fgDim} wrapMode="none">{` #${s.pr}`}</text>
              </box>
            ) : null}
          </box>
        );
      })}
    </>
  );
}

/** The manual-section member list (status · label · badges), mirroring
 *  the StackChain row format minus the spine — manual members have no
 *  dependency relationships, so there's no tree to draw. The right side
 *  is the shared list-pane badge cluster, identical per row. */
function SectionMembers({ members }: { members: SectionMember[] }) {
  // Status breakdown in StatusKind declaration order, non-zero kinds
  // only — the kind values double as display words ("dirty", "clean").
  const breakdown = Object.values(StatusKind)
    .map((k) => ({
      k,
      n: members.filter((m) => m.row.status.kind === k).length,
    }))
    .filter(({ n }) => n > 0)
    .map(({ k, n }) => `${n} ${k}`)
    .join(" · ");
  return (
    <>
      <text fg={theme.fgDim} wrapMode="none" truncate>
        {breakdown || "no worktrees"}
      </text>
      <box height={1} flexShrink={0} />
      {members.map((m) => {
        const b = statusBadge(m.row.status);
        const dim = m.row.archived;
        return (
          <box key={m.row.wt.slug} flexDirection="row">
            <box width={2} flexShrink={0}>
              <text fg={dim ? theme.fgDim : b.fg} wrapMode="none">{b.glyph}</text>
            </box>
            <box width={1} flexShrink={0}>
              <text> </text>
            </box>
            <box flexGrow={1} flexShrink={1} overflow="hidden">
              <text
                fg={dim ? theme.fgDim : theme.fg}
                wrapMode="none"
                truncate
              >
                {m.label}
              </text>
            </box>
            <BadgeCluster
              row={m.row}
              actionRunning={m.actionRunning}
              activeHarnessId={m.activeHarnessId}
              sessionState={m.sessionState}
            />
          </box>
        );
      })}
    </>
  );
}

/** Detail-pane body for a folded section header (stack or manual section). */
function SectionSummaryBody({ section, width }: { section: SectionDetail; width: number }) {
  return (
    <box
      flexGrow={1}
      width={width}
      flexShrink={0}
      border
      borderStyle="single"
      borderColor={theme.border}
      title={section.isStack ? " stack " : " section "}
      titleAlignment="left"
      padding={1}
    >
      <box flexShrink={0} overflow="hidden">
        <text fg={theme.fgBright} attributes={TextAttributes.BOLD} wrapMode="none" truncate>
          {section.label}
        </text>
      </box>
      <box height={1} flexShrink={0} />
      {section.manifest ? (
        <StackChain manifest={section.manifest} members={section.members} />
      ) : (
        <SectionMembers members={section.members} />
      )}
      <box flexGrow={1} flexShrink={1} minHeight={0} />
      <text fg={theme.fgDim} wrapMode="none">TAB to expand</text>
    </box>
  );
}

export function Details({ row, reviewRequest, section, width, scrollRef }: Props) {
  if (section) {
    return <SectionSummaryBody key={`section:${section.sectionKey}`} section={section} width={width} />;
  }
  if (reviewRequest) {
    // Key by url so navigating across review-request rows remounts
    // cleanly — no chance of bleeding state from one PR to another.
    return (
      <ReviewRequestBody
        key={reviewRequest.url}
        pr={reviewRequest}
        width={width}
        scrollRef={scrollRef}
      />
    );
  }
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
  // Key by slug so the AI summary observer below resets across
  // worktree switches. Without this, `placeholderData: keepPreviousData`
  // bleeds the previous slug's summary into the new slug whenever the
  // new slug has no cache entry (cold key, or disabled because no diff
  // context yet) — so navigating A → B parks A's description on B until
  // B's own fetch lands or the user restarts.
  return (
    <DetailsBody
      key={row.wt.slug}
      row={row}
      width={width}
      scrollRef={scrollRef}
    />
  );
}
