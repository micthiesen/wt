import { useEffect } from "react";
import { CliRenderEvents } from "@opentui/core";
import { useRenderer } from "@opentui/react";

import { logErr, logInfo } from "../events.ts";
import { writeClipboard } from "../helpers.ts";

type SelectionLike = { getSelectedText(): string };

function extractSelection(selection: unknown): string | null {
  if (
    selection &&
    typeof selection === "object" &&
    typeof (selection as SelectionLike).getSelectedText === "function"
  ) {
    try {
      return (selection as SelectionLike).getSelectedText() || null;
    } catch {
      return null;
    }
  }
  return null;
}

/**
 * Auto-copy-on-select. Subscribes to the renderer's `selection` event,
 * which fires once per drag when the user releases the mouse. Copies
 * the selected text to the system clipboard and logs to the activity
 * pane. Selection stays visible until the user clicks elsewhere or
 * presses Esc.
 */
export function useAutoCopy(): void {
  const renderer = useRenderer();
  useEffect(() => {
    if (!renderer) return;
    const handler = (selection: unknown): void => {
      const text = extractSelection(selection);
      if (!text) return;
      try {
        writeClipboard(text);
      } catch (err) {
        logErr("app", `pbcopy failed: ${err instanceof Error ? err.message : String(err)}`);
        return;
      }
      const lines = text.split("\n").length;
      const suffix = lines > 1 ? ` (${lines} lines)` : "";
      logInfo("app", `copied ${text.length} chars${suffix}`);
    };
    renderer.on(CliRenderEvents.SELECTION, handler);
    return () => {
      renderer.off(CliRenderEvents.SELECTION, handler);
    };
  }, [renderer]);
}
