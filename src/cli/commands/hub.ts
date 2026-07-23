/**
 * `wt hub` — launch (or re-attach to) the hub layout: an outer tmux
 * server hosting the task-inbox pane on the left and a live harness
 * pane on the right. See docs/hub.md. Also the target of a bare `wt`
 * when `[ui] mode = "hub"` is configured.
 */
export async function run(_argv: string[]): Promise<number> {
  const { isHubPane, launchHub } = await import("../../core/hub.ts");
  if (isHubPane()) {
    console.error("already inside the hub — this command attaches from a plain terminal");
    return 2;
  }
  return launchHub();
}
