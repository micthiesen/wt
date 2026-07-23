/**
 * Track whether THIS process's tmux pane has keyboard focus — hub mode
 * only. Two signal sources, deterministic one first:
 *
 *  1. wt's own focus mutations. F9 is forwarded into the task pane
 *     (like F10-F12) and wt runs the `select-pane` itself; the modal
 *     focus dance likewise calls focusLeft/focusRight. Every one of
 *     those call sites stamps the state via `setFocused`, so the
 *     indicator can never drift from an action wt performed.
 *  2. Terminal focus events as the fallback for changes wt did NOT
 *     make (a mouse click on a pane): the outer hub server runs
 *     `focus-events on`, tmux forwards XTerm focus-in/out to the pane,
 *     and opentui re-emits them as renderer FOCUS / BLUR.
 *
 * Starts `true`: `ensureHubLayout` selects the task pane at creation,
 * so the hub opens with the inbox focused.
 */
import { useEffect, useState } from "react";
import { CliRenderEvents } from "@opentui/core";
import { useRenderer } from "@opentui/react";

export function useHubPaneFocus(enabled: boolean): {
  focused: boolean;
  setFocused: (focused: boolean) => void;
} {
  const renderer = useRenderer();
  const [focused, setFocused] = useState(true);
  useEffect(() => {
    if (!enabled || !renderer) return;
    const onFocus = (): void => setFocused(true);
    const onBlur = (): void => setFocused(false);
    renderer.on(CliRenderEvents.FOCUS, onFocus);
    renderer.on(CliRenderEvents.BLUR, onBlur);
    return () => {
      renderer.off(CliRenderEvents.FOCUS, onFocus);
      renderer.off(CliRenderEvents.BLUR, onBlur);
    };
  }, [enabled, renderer]);
  return { focused, setFocused };
}
