/**
 * Derived data for the sessions picker modal: the picker row list for
 * the modal's slug and the per-session summary map for its bottom
 * panel. Extracted from `app.tsx` — pure derivation over the modal
 * state + the current row's harness sessions.
 */
import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";

import { HARNESSES } from "../../core/harness/index.ts";
import { claudeSummariesQuery } from "../../state/index.ts";
import type { Modal } from "../modal.ts";
import type { PickerRow } from "../panels/sessions-picker.tsx";
import type { useHarnessSessions } from "./useHarnessSessions.ts";
import type { WorktreeRow } from "./useWorktreeRows.ts";

export function useSessionsPickerData(opts: {
  modal: Modal | null;
  rows: WorktreeRow[];
  currentHarnessSessions: ReturnType<typeof useHarnessSessions>;
}) {
  const { modal, rows, currentHarnessSessions } = opts;

  // LLM-authored summary snippets for the picker's currently-open
  // worktree. Only fetched when the picker is open (gated by
  // `enabled`); the queryFn does light tail-bounded disk reads
  // cached by (mtime, size) so repeat opens are essentially free.
  const pickerWt = (modal?.kind === "claudeSessionsPicker"
    ? rows.find((r) => r.wt.slug === modal.slug)?.wt
    : undefined);
  const pickerWtForQuery = pickerWt ?? { slug: "__none__", path: "" };
  const summariesQuery = useQuery({
    ...claudeSummariesQuery(pickerWtForQuery),
    enabled: !!pickerWt,
  });
  // Sessions-picker rows for the current modal slug. Built from
  // `currentHarnessSessions` so claude/codex/opencode entries surface
  // in one list. Trailing "+ new" affordances are appended one per
  // harness so per-harness letters (`c`/`o`/`x`) land on distinct
  // rows. Index space: [sessions...] [new-claude] [new-codex] [new-opencode].
  const pickerSlug =
    modal?.kind === "claudeSessionsPicker" ? modal.slug : null;
  const pickerRows = useMemo<ReadonlyArray<PickerRow>>(() => {
    if (pickerSlug === null) return [];
    // `sessions` is already sorted live-first then recency-desc by
    // `compareSessionsForDisplay` inside the hook.
    const out: PickerRow[] = currentHarnessSessions.sessions.map((entry) => ({
      kind: "session",
      entry,
    }));
    for (const h of HARNESSES) {
      out.push({ kind: "new", harnessId: h.id });
    }
    return out;
  }, [pickerSlug, currentHarnessSessions.sessions]);
  // Summaries keyed by session id for the picker's bottom panel.
  // Claude-only today; codex / opencode entries fall back to the
  // "(no summary yet)" placeholder.
  const pickerSummaries = useMemo(() => {
    const m = new Map<string, { text: string } | null>();
    const raw = summariesQuery.data ?? {};
    for (const [id, value] of Object.entries(raw)) {
      m.set(id, value);
    }
    return m;
  }, [summariesQuery.data]);

  return { pickerRows, pickerSummaries };
}
