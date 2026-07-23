/**
 * Track whether THIS process's tmux pane has keyboard focus — hub mode
 * only. Three signal sources, most deterministic first:
 *
 *  1. wt's own focus mutations. F9 is forwarded into the task pane
 *     (like F10-F12) and wt runs the `select-pane` itself; the modal
 *     focus dance calls focusLeft/focusRight; F8/⌘F zoom runs through
 *     `toggleSessionZoom`. Every one of those call sites stamps the
 *     state via `setFocused`, so the indicator can never drift from an
 *     action wt performed.
 *  2. Terminal focus events for changes wt did NOT make (a mouse click
 *     on a pane): the outer hub server runs `focus-events on`, tmux
 *     forwards XTerm focus-in/out to the pane, and opentui re-emits
 *     them as renderer FOCUS / BLUR.
 *  3. A slow ground-truth reconciler: tmux does not emit focus events
 *     for every active-pane change (zoom-induced switches were the
 *     observed gap), so every few seconds the hook asks the outer
 *     server directly (`#{pane_active}`) and corrects any drift. The
 *     poll defers to a fresh explicit stamp (source 1) so it can't
 *     race a queued select-pane and flap the indicator; per the
 *     freshness model this bounds how long the indicator can be wrong
 *     rather than being the mechanism.
 *
 * Starts `true`: `ensureHubLayout` selects the task pane at creation,
 * so the hub opens with the inbox focused.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { CliRenderEvents } from "@opentui/core";
import { useRenderer } from "@opentui/react";

import { isLeftPaneActive } from "../../core/hub.ts";

/** How often the ground-truth reconciler polls the outer server. */
const RECONCILE_INTERVAL_MS = 3_000;

/** How long after an explicit stamp the reconciler holds off (a queued select-pane may not have applied yet). */
const RECONCILE_HOLDOFF_MS = 1_500;

export function useHubPaneFocus(enabled: boolean): {
  focused: boolean;
  setFocused: (focused: boolean) => void;
} {
  const renderer = useRenderer();
  const [focused, setFocusedState] = useState(true);
  const lastStampRef = useRef(0);

  // Explicit stamps (wt-performed moves) record their time so the
  // reconciler yields to them during the apply window.
  const setFocused = useCallback((next: boolean): void => {
    lastStampRef.current = Date.now();
    setFocusedState(next);
  }, []);

  useEffect(() => {
    if (!enabled || !renderer) return;
    const onFocus = (): void => setFocusedState(true);
    const onBlur = (): void => setFocusedState(false);
    renderer.on(CliRenderEvents.FOCUS, onFocus);
    renderer.on(CliRenderEvents.BLUR, onBlur);
    return () => {
      renderer.off(CliRenderEvents.FOCUS, onFocus);
      renderer.off(CliRenderEvents.BLUR, onBlur);
    };
  }, [enabled, renderer]);

  useEffect(() => {
    if (!enabled) return;
    let cancelled = false;
    const tick = async (): Promise<void> => {
      if (Date.now() - lastStampRef.current < RECONCILE_HOLDOFF_MS) return;
      const active = await isLeftPaneActive();
      if (cancelled || active === null) return;
      // Re-check the holdoff after the await — an explicit stamp may
      // have landed while the query was in flight.
      if (Date.now() - lastStampRef.current < RECONCILE_HOLDOFF_MS) return;
      setFocusedState((cur) => (cur === active ? cur : active));
    };
    const timer = setInterval(() => void tick(), RECONCILE_INTERVAL_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [enabled]);

  return { focused, setFocused };
}
