import { useMemo } from "react";
import { keepPreviousData, useQueries, useQuery } from "@tanstack/react-query";

import { config } from "../../core/config.ts";
import { slugLabel } from "../../core/stage.ts";
import {
  stackTitleQuery,
  wtStateQuery,
  type StackMember,
} from "../../state/index.ts";
import type { WorktreeRow } from "./useWorktreeRows.ts";

export function useStackSections(rows: readonly WorktreeRow[]) {
  // Stack section AI title pipeline. The title describes section
  // membership, so it reads from `rows` directly. Members preserve
  // `rows` order (chain depth from the row aggregator).
  const stackSectionMembers = useMemo((): Map<
    string,
    { members: StackMember[]; ready: boolean; rootSlug: string | null; firstSlug: string }
  > => {
    const byName = new Map<
      string,
      { members: StackMember[]; ready: boolean; rootSlug: string | null; firstSlug: string }
    >();
    for (const r of rows) {
      if (!r.sectionIsStack || r.section === null) continue;
      // A detached HEAD wouldn't legitimately be part of a stack
      // (stacks walk branch-parent chains), but the type allows it.
      // Skipping keeps the signature stable and the prompt clean.
      if (!r.wt.branch) continue;
      const { id, rest } = slugLabel(r.wt.slug);
      // Brief is the LLM's pithy noun phrase, falling back to the
      // slug-derived label. `ready` flips false while any member still
      // lacks a real brief because the title fetch is gated on it.
      const brief = r.brief ?? (rest || id || r.wt.slug);
      let entry = byName.get(r.section);
      if (!entry) {
        entry = { members: [], ready: true, rootSlug: null, firstSlug: r.wt.slug };
        byName.set(r.section, entry);
      }
      entry.members.push({ branch: r.wt.branch, brief });
      if (r.stack?.depth === 0) entry.rootSlug = r.wt.slug;
      if (r.brief == null) entry.ready = false;
    }
    return byName;
  }, [rows]);

  const stackSectionEntries = useMemo(
    () => Array.from(stackSectionMembers.entries()),
    [stackSectionMembers],
  );

  // `placeholderData: keepPreviousData` so adding/removing a member
  // mid-session doesn't flicker the divider through the storage name
  // while the new key fetches.
  const stackTitleResults = useQueries({
    queries: stackSectionEntries.map(([name, { members, ready }]) => ({
      ...stackTitleQuery(name, members),
      // Hold generation until every member has a real LLM brief. Titles
      // cache forever under the membership signature, so a cold fire
      // with slug-fallback briefs bakes in a weak title.
      enabled: ready && members.length > 0 && !!config.ai,
      placeholderData: keepPreviousData,
    })),
  });

  // Every inferred stack gets a label entry (the root's issue id, from
  // its slug); the AI title, when resolved, is woven in between.
  const wtStateForStacks = useQuery(wtStateQuery());
  const foldedSections = useMemo<ReadonlySet<string>>(
    () => new Set(wtStateForStacks.data?.foldedSections ?? []),
    [wtStateForStacks.data?.foldedSections],
  );
  const stackSectionLabels = useMemo((): Map<string, string> => {
    const m = new Map<string, string>();
    for (let i = 0; i < stackSectionEntries.length; i++) {
      const [name, { rootSlug, firstSlug }] = stackSectionEntries[i]!;
      const title = stackTitleResults[i]?.data;
      // The root's issue id, falling back to the first member's (an
      // archived root loses its `stack` info and never registers as
      // depth 0) and finally a generic marker.
      const issue = slugLabel(rootSlug ?? firstSlug).id;
      const label = issue || "stack";
      m.set(
        name,
        typeof title === "string" && title.trim() !== ""
          ? `${label} · ${title}`
          : label,
      );
    }
    return m;
  }, [stackSectionEntries, stackTitleResults]);

  return { wtStateForStacks, foldedSections, stackSectionLabels };
}
