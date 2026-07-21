import type { CliRenderer } from "@opentui/core";

import { config } from "../../core/config.ts";
import type { HarnessId } from "../../core/harness/index.ts";
import type { RemoteWorktreeSummary } from "../../core/remote-worktrees.ts";
import { runRemoteWt } from "../../core/remote.ts";
import { setWezTermTabTitle } from "../../core/wezterm.ts";
import { NF } from "../icons.ts";
import { handoffTerminal } from "./renderer-handoff.ts";

/** Hand the terminal to one selected remote worktree's tmux session. */
export async function enterRemoteWorktreeSession(opts: {
  renderer: CliRenderer;
  worktree: RemoteWorktreeSummary;
  target: "shell" | "diff" | "harness";
  harnessId: HarnessId;
}): Promise<number> {
  const remote = config.remote;
  if (!remote) throw new Error("[remote] is not configured in config.toml");
  const { renderer, worktree, target, harnessId } = opts;
  await setWezTermTabTitle(
    `${NF.remote} ${worktree.slug} · ${remote.label}`,
    config.paths.weztermCli,
  );
  try {
    return await handoffTerminal(renderer, process.cwd(), () =>
      runRemoteWt(remote, ["_session", worktree.slug, target, harnessId], {
        interactive: true,
      }),
    );
  } finally {
    await setWezTermTabTitle("wt", config.paths.weztermCli);
  }
}
