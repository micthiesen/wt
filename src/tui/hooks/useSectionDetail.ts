import { useMemo } from "react";

import type { WtState } from "../../core/wtstate.ts";
import { STACK_SECTION_PREFIX } from "../../core/wtstate.ts";
import type { ActiveSessionGlyph } from "./useHarnessSessions.ts";
import type { SectionDetail } from "../panels/details.tsx";
import { rowLabel } from "../panels/list.tsx";
import type { SelectedSection } from "./useVisualItems.ts";

type UseSectionDetailArgs = {
  selectedSection: SelectedSection | undefined;
  wtState: WtState | undefined;
  activeActions: ReadonlySet<string>;
  activeSessionBySlug: ReadonlyMap<string, ActiveSessionGlyph>;
};

export function useSectionDetail({
  selectedSection,
  wtState,
  activeActions,
  activeSessionBySlug,
}: UseSectionDetailArgs): SectionDetail | undefined {
  return useMemo<SectionDetail | undefined>(() => {
    if (!selectedSection) return undefined;
    const stackId = selectedSection.isStack
      ? selectedSection.sectionKey.slice(STACK_SECTION_PREFIX.length)
      : null;
    return {
      sectionKey: selectedSection.sectionKey,
      isStack: selectedSection.isStack,
      label: selectedSection.label,
      automationsPaused:
        stackId !== null && (wtState?.pausedStacks ?? []).includes(stackId),
      members: selectedSection.rows.map((r) => ({
        label: rowLabel(r),
        row: r,
        actionRunning: activeActions.has(r.wt.slug),
        activeHarnessId: activeSessionBySlug.get(r.wt.slug)?.harnessId,
        sessionState: activeSessionBySlug.get(r.wt.slug)?.state ?? undefined,
      })),
    };
  }, [selectedSection, wtState, activeActions, activeSessionBySlug]);
}
