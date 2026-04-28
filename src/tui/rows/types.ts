/**
 * Row module contract for the details pane.
 *
 * A row module is a small declarative bundle: an id (matched against
 * `config.ui.rows`), a label for the left gutter, the set of query
 * sources its content depends on, and a renderer.
 *
 * The driver in `panels/details.tsx` handles the things that should
 * never live inside a row: ordering, the trailing staleness glyph,
 * and inline error display once retries are exhausted.
 *
 * Pure-derived rows (branch, path, linear) declare no sources. They
 * never show staleness or errors because there's no fetch behind
 * them — the data comes straight off the worktree record.
 */
import type { UseQueryResult } from "@tanstack/react-query";
import type { ReactNode } from "react";

import type { GithubData } from "../../state/queries.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";

/**
 * Subset of a query/field state that the driver needs. Both
 * `FieldState` (from useWorktreeRows) and `UseQueryResult` (from
 * react-query) satisfy this without adapters.
 */
export type FetchLike = {
  data?: unknown;
  isFetching: boolean;
  error?: Error | null;
};

export type RowContext = {
  row: WorktreeRow;
  github: UseQueryResult<GithubData, Error>;
};

export type RowModule = {
  id: string;
  label: string;
  /**
   * Sources whose fetch state drives the row's staleness glyph and
   * error display. Order matters: the *first* errored source wins
   * the inline error slot.
   */
  sources?: (ctx: RowContext) => FetchLike[];
  /** Return false to hide the row entirely (no slot reserved). */
  visible?: (ctx: RowContext) => boolean;
  /** Render the value content (label and trailing glyph come from the driver). */
  render: (ctx: RowContext) => ReactNode;
};
