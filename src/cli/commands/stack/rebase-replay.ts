import { config } from "../../../core/config.ts";
import { rebaseStack, reconcileStack, replayStack } from "../../../core/stack-ops.ts";
import { getStackManifest } from "../../../core/wtstate.ts";
import { bold, green, red } from "../../colors.ts";
import { logLine, parseStackTarget, reportReplayResult } from "./shared.ts";

export async function runRebase(argv: string[]): Promise<number> {
  const t = await parseStackTarget(argv, "rebase");
  if (typeof t === "number") return t;
  const opts = t.onto ? { onto: t.onto } : {};
  return reportReplayResult(t.stackId, "rebased", await rebaseStack(t.stackId, opts, logLine));
}

export async function runReplay(argv: string[]): Promise<number> {
  const t = await parseStackTarget(argv, "replay");
  if (typeof t === "number") return t;
  const opts = t.onto ? { onto: t.onto } : {};
  return reportReplayResult(t.stackId, "replayed", await replayStack(t.stackId, opts, logLine));
}

export async function runReconcile(argv: string[]): Promise<number> {
  const t = await parseStackTarget(argv, "reconcile");
  if (typeof t === "number") return t;
  if (!getStackManifest(t.stackId)) {
    console.error(red(`no stack manifest: ${t.stackId}`));
    return 1;
  }
  await reconcileStack(t.stackId, t.onto ?? config.branch.base, logLine);
  console.log(green(`✓ reconciled ${bold(t.stackId)}`));
  return 0;
}
