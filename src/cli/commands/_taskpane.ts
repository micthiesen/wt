/**
 * `wt _taskpane` — internal entrypoint the hub layout runs in its left
 * pane: the TUI in hub mode (task inbox + right-pane control, no
 * details/output panes of its own). Not for direct use; `wt hub` is
 * the user-facing command that builds the layout around this.
 */
export async function run(_argv: string[]): Promise<number> {
  const { runTui } = await import("../../tui/runtime.tsx");
  await runTui({ hubPane: true });
  return 0;
}
