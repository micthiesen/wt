/**
 * `wt _taskpane` — internal entrypoint the hub layout runs in its left
 * pane: the TUI in hub mode (task inbox + right-pane control). It does
 * render a details card of its own (the `I` binding toggles it) — the
 * one thing it lacks versus the classic TUI is an OutputViewer, since
 * the hub's right pane (a separate harness/tmux session) is where
 * session output actually lives. Not for direct use; `wt hub` is the
 * user-facing command that builds the layout around this.
 */
export async function run(_argv: string[]): Promise<number> {
  // Re-skin to the terminal's theme before anything renders — the task
  // pane sits beside a harness on the terminal's own background, so it
  // adopts that palette (see applyHubPalette).
  const { applyHubPalette } = await import("../../tui/theme.ts");
  applyHubPalette();
  // Load the persisted focus stamps before the TUI mounts: this
  // process (the hub pane) is the sole writer of task-focus.json (see
  // core/task-focus.ts's header), so it's also the only process that
  // should ever pull it off disk — a plain getSnapshot() no longer
  // does this lazily (that was the multi-process purity bug the
  // explicit load() was added to fix).
  const { taskFocusStore } = await import("../../core/task-focus.ts");
  taskFocusStore.load();
  const { runTui } = await import("../../tui/runtime.tsx");
  await runTui({ hubPane: true });
  return 0;
}
