/**
 * Foreground color for one `ActionLine` kind. Shared between the
 * action / session output viewer (multi-line lists in the bottom pane)
 * and the bottom-bar tail (single line in the footer) so both surfaces
 * agree on per-envelope coloring.
 */
import type { ActionLine } from "../core/harness/claude/events.ts";

import { theme } from "./theme.ts";

export function actionLineFg(kind: ActionLine["kind"]): string {
  switch (kind) {
    case "info":
      return theme.fgDim;
    case "user":
      return theme.accent;
    case "assistant":
      return theme.fg;
    case "thinking":
      return theme.fgDim;
    case "tool":
      // In-flight — dim, intentionally muted so a column of pending
      // tool calls reads as "background activity" while the eye gets
      // drawn to whatever's actively resolving below them.
      return theme.fgDim;
    case "tool-ok":
      return theme.ok;
    case "tool-err":
      return theme.err;
    case "stdout":
      return theme.fg;
    case "stderr":
      return theme.warn;
    case "exit-success":
      return theme.ok;
    case "exit-failure":
      return theme.err;
  }
}
