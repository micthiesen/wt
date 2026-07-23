/**
 * Track whether THIS process's tmux pane has keyboard focus — hub mode
 * only. The outer hub server runs `focus-events on`, so tmux forwards
 * XTerm focus-in/out to the pane whenever the active pane changes (F9,
 * `select-pane`, a mouse click), and opentui re-emits them as renderer
 * FOCUS / BLUR events. Classic mode gets window-level focus through the
 * same events (see `useTerminalFocus`); in a hub pane they mean pane
 * focus, which is exactly the "where does typing go" signal the task
 * pane surfaces.
 *
 * Starts `false`: `ensureHubLayout` selects the right (session) pane at
 * creation, and terminals don't replay focus state — the first
 * transition corrects it either way.
 */
import { useEffect, useState } from "react";
import { CliRenderEvents } from "@opentui/core";
import { useRenderer } from "@opentui/react";

export function useHubPaneFocus(enabled: boolean): boolean {
  const renderer = useRenderer();
  const [focused, setFocused] = useState(false);
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
  return focused;
}
