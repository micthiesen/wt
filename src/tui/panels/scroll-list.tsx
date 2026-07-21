import { useEffect, useRef, type ReactNode } from "react";
import type { ScrollBoxRenderable } from "@opentui/core";

import { useScrollbarNoFlash } from "../hooks/useScrollbarNoFlash.ts";

type Props = {
  /**
   * Stable `id` of the currently-selected child row. When it changes the
   * list scrolls that row into view, so j/k navigation past the fold
   * keeps the cursor on screen instead of clipping at the modal's bottom
   * edge. Rows must carry a matching `id` prop. Omit for a scroll region
   * with no cursor (e.g. a confirm list or a text preview).
   */
  selectedId?: string;
  /**
   * Extra value that should also re-run scroll-into-view when it changes
   * — typically the items array (a rebuild re-anchors the cursor) or the
   * text being typed (so the input stays visible as it grows).
   */
  revision?: unknown;
  children: ReactNode;
};

/**
 * Vertical-scroll wrapper for modal / picker lists whose content can
 * exceed the modal height. Fills its parent (`flexGrow`), suppresses the
 * mount scrollbar flash, and scrolls the selected row into view as the
 * cursor moves — the shared version of the pattern first used in
 * `removed-list.tsx`. The `Modal` shell clips overflow with no scrollback
 * of its own, so any list that maps unbounded user data (actions,
 * sessions, branches, outputs, clean candidates) must wrap it in this or
 * rows past the fold become unreachable.
 *
 * Rows keep owning their own HORIZONTAL truncation (`wrapMode="none"
 * truncate` inside a `flexGrow`/`flexShrink`/`overflow="hidden"` box);
 * this only handles the vertical axis.
 */
export function ScrollableList({ selectedId, revision, children }: Props) {
  const listRef = useRef<ScrollBoxRenderable>(null);
  const scrollRef = useScrollbarNoFlash(listRef);
  useEffect(() => {
    if (selectedId) listRef.current?.scrollChildIntoView(selectedId);
  }, [selectedId, revision]);
  return (
    <scrollbox
      ref={scrollRef}
      scrollY
      flexGrow={1}
      minHeight={0}
      contentOptions={{ flexDirection: "column" }}
    >
      {children}
    </scrollbox>
  );
}
