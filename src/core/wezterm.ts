/**
 * WEZTERM_PANE is set by WezTerm for local panes and inherited through
 * multiplexers such as tmux. Unlike TERM_PROGRAM, it also identifies the pane
 * that `wezterm cli` should use to find the containing tab.
 */
export function isRunningInWezTerm(env: NodeJS.ProcessEnv = process.env): boolean {
  return Boolean(env.WEZTERM_PANE);
}

export function wezTermCliPath(
  configuredPath: string | null,
  which: (name: string) => string | null = Bun.which,
): string | null {
  return configuredPath ?? which("wezterm");
}

/** Set the containing WezTerm tab's explicit title. Failure is non-fatal. */
export async function setWezTermTabTitle(
  title: string,
  configuredCliPath: string | null,
): Promise<void> {
  if (!isRunningInWezTerm()) return;

  const wezterm = wezTermCliPath(configuredCliPath);
  if (!wezterm) return;

  try {
    const proc = Bun.spawn([wezterm, "cli", "set-tab-title", title], {
      stdin: "ignore",
      stdout: "ignore",
      stderr: "ignore",
    });
    await proc.exited;
  } catch {
    // Tab naming is cosmetic and must never prevent wt from starting.
  }
}
