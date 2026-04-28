import { useEffect, useRef } from "react";
import { CliRenderEvents } from "@opentui/core";
import { useRenderer } from "@opentui/react";

/**
 * Subscribe to terminal focus-gain events. opentui requests XTerm focus
 * tracking during `setupTerminal` and emits `focus` / `blur` on the
 * renderer when the terminal window gains or loses focus. We only care
 * about focus-in — it's the "I'm looking at the TUI again" signal.
 *
 * No throttling: the caller is expected to do cheap, idempotent work
 * on focus (e.g. "refetch stale queries"), which is self-limiting —
 * nothing re-runs if nothing is stale.
 */
export function useTerminalFocus(handler: () => void): void {
  const renderer = useRenderer();
  const handlerRef = useRef(handler);
  handlerRef.current = handler;
  useEffect(() => {
    if (!renderer) return;
    const onFocus = (): void => {
      handlerRef.current();
    };
    renderer.on(CliRenderEvents.FOCUS, onFocus);
    return () => {
      renderer.off(CliRenderEvents.FOCUS, onFocus);
    };
  }, [renderer]);
}
