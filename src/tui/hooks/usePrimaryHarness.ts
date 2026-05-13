/**
 * Read the persisted primary-harness selection. The query is cached
 * with `staleTime: Infinity`; mutations bump it via
 * `useWtActions().setPrimaryHarness` / `cyclePrimaryHarness` which
 * invalidate explicitly. Default `claude` is returned during the
 * brief loading window so the UI never flashes a placeholder.
 */
import { useQuery } from "@tanstack/react-query";

import type { HarnessId } from "../../core/harness/index.ts";
import { primaryHarnessQuery } from "../../state/queries.ts";

export function usePrimaryHarness(): HarnessId {
  const q = useQuery(primaryHarnessQuery());
  return q.data ?? "claude";
}
