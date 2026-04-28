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
import { config } from "../../core/config.ts";
import { useGithub } from "../../state/hooks.ts";
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

export function Details({ row }: Props) {
  // Subscribe to the combined GitHub fetch so per-row indicators
  // reflect its fetch state. Observers dedupe by key — this doesn't
  // trigger an extra fetch, it joins the existing observer in
  // `useWorktreeRows`. Hook order is stable because we always
  // subscribe regardless of the `row` prop.
  const github = useGithub();

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

  const ctx: RowContext = { row, github };

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
      {RESOLVED_ROWS.map((m) => (
        <RenderedRow key={m.id} module={m} ctx={ctx} />
      ))}
    </box>
  );
}
