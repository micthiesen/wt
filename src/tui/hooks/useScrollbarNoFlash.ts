import { useCallback, useRef, type RefObject } from "react";
import { useRenderer } from "@opentui/react";
import type { ScrollBoxRenderable } from "@opentui/core";

/**
 * Suppress the one-frame scrollbar "flash" a `<scrollbox>` shows on mount.
 *
 * OpenTUI's auto-visibility rule is `visible = scrollSize > viewportSize`,
 * recomputed whenever either changes. But a freshly-mounted box has
 * `viewportSize === 0` until its first layout pass, so any non-empty
 * content trips `scrollSize > 0` and flashes the bar on for a frame
 * before the viewport is measured and it hides again. Visible on the
 * list at startup and on every details-pane swap (the body remounts).
 *
 * We manually hide both bars the instant the box attaches — which makes
 * `recalculateVisibility` a no-op while `_manualVisibility` is set — then
 * hand control back to auto via `resetVisibilityControl()` on the first
 * frame where a real viewport height exists. Net effect: the bar only
 * appears when the content genuinely overflows, never as a flash.
 *
 * Frame callbacks run before the frame's layout pass, so `viewportSize`
 * is still 0 on the mount frame; the poll guard waits the one extra
 * frame until layout has sized the viewport before restoring auto.
 *
 * Returns a callback ref to put on the `<scrollbox>`. Pass `forwardRef`
 * to also populate an external ref (e.g. for imperative paging).
 */
export function useScrollbarNoFlash(
  forwardRef?: RefObject<ScrollBoxRenderable | null>,
): (node: ScrollBoxRenderable | null) => void {
  const renderer = useRenderer();
  const pendingRef = useRef<((dt: number) => Promise<void>) | null>(null);
  return useCallback(
    (node: ScrollBoxRenderable | null) => {
      if (forwardRef) forwardRef.current = node;
      // Cancel a restore still pending from a previous mount.
      if (pendingRef.current) {
        renderer.removeFrameCallback(pendingRef.current);
        pendingRef.current = null;
      }
      if (!node) return;
      node.verticalScrollBar.visible = false;
      node.horizontalScrollBar.visible = false;
      const restore = async () => {
        // Wait for the first layout to size the viewport, then return
        // both bars to auto-visibility exactly once.
        if (node.verticalScrollBar.viewportSize <= 0) return;
        node.verticalScrollBar.resetVisibilityControl();
        node.horizontalScrollBar.resetVisibilityControl();
        if (pendingRef.current) {
          renderer.removeFrameCallback(pendingRef.current);
          pendingRef.current = null;
        }
      };
      pendingRef.current = restore;
      renderer.setFrameCallback(restore);
    },
    [renderer, forwardRef],
  );
}
