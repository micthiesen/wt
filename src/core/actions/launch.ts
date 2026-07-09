import { join } from "node:path";

import type { ToolStartMap } from "../harness/claude/events.ts";
import { config } from "../config.ts";
import type { HarnessId } from "../harness/index.ts";
import type { ActionRunKind } from "./types.ts";

export function actionsDir(): string {
  return join(config.paths.logDir, "actions");
}

export function headlessPromptRunner(
  harnessId: HarnessId,
  prompt: string,
  cwd: string,
): { kind: ActionRunKind; argv: string[] } {
  switch (harnessId) {
    case "claude":
      return {
        kind: "claude",
        argv: [
          "claude",
          "-p",
          "--permission-mode",
          "auto",
          "--verbose",
          "--output-format",
          "stream-json",
          prompt,
        ],
      };
    case "codex":
      return {
        kind: "harness",
        argv: ["codex", "exec", "--color", "never", "--", prompt],
      };
    case "opencode":
      return {
        kind: "harness",
        argv: ["opencode", "run", "--dir", cwd, "--", prompt],
      };
    default: {
      const _exhaustive: never = harnessId;
      throw new Error(`unhandled harness id: ${String(_exhaustive)}`);
    }
  }
}

/** Filesystem-safe per-run directory id: `<slug>-<iso>` with `:`/`.`
 *  replaced. Stable across reads of the same run; distinct across
 *  runs even on the same slug. */
export function formatRunId(slug: string, startedAt: number): string {
  return `${slug}-${new Date(startedAt).toISOString().replace(/[:.]/g, "-")}`;
}

export function makeFreshHandles(): {
  toolStarts: ToolStartMap;
  resultEventSeen: boolean;
} {
  return { toolStarts: new Map(), resultEventSeen: false };
}
