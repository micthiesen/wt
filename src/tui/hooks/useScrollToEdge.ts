import { useEffect, type RefObject } from "react";
import type { ScrollBoxRenderable } from "@opentui/core";

/**
 * Imperative scroll-to-edge control shape both list panels expose to
 * their parent's j/k handler — identical to `ListScrollHandle`
 * (`panels/list.tsx`) and `TaskListHandle` (`panels/tasks.tsx`), which
 * keep their own named aliases for the public prop but are structurally
 * this same shape.
 */
export type ScrollToEdgeHandle = { toEdge: (dir: "top" | "bottom") => void };

/**
 * Wires `scrollHandle` (the `RefObject` a parent passed down as a prop)
 * to an imperative `toEdge` control backed by `listRef`'s live
 * `ScrollBoxRenderable`. A large `scrollBy` clamps at the content edge,
 * so `toEdge` reveals whatever trailing content the cursor itself can't
 * reach — blank space, or headers below the last selectable row.
 *
 * `panels/list.tsx` and `panels/tasks.tsx` both wire their own
 * `<scrollbox>` ref through this so the parent's j/k "jump to edge"
 * handler can treat either pane identically without either panel
 * hand-rolling the same effect.
 */
export function useScrollToEdge(
  listRef: RefObject<ScrollBoxRenderable | null>,
  scrollHandle: RefObject<ScrollToEdgeHandle | null> | undefined,
): void {
  useEffect(() => {
    if (!scrollHandle) return;
    scrollHandle.current = {
      toEdge: (dir) => listRef.current?.scrollBy(dir === "bottom" ? 9999 : -9999, "viewport"),
    };
    return () => {
      if (scrollHandle) scrollHandle.current = null;
    };
    // `listRef` deliberately excluded: it's a `useRef` object, stable
    // across renders, and the original inline effect in both panels
    // only ever depended on `scrollHandle`.
  }, [scrollHandle]);
}
