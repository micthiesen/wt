const KEYS_USAGE = `usage: wt hub keys <terminal>

Print a ready-to-paste command-layer config snippet for <terminal> —
the cmd+<key> chord table that drives hub mode from outside the task
pane (see docs/hub.md#the-command-layer).

terminals:
  alacritty   [keyboard] bindings entries for alacritty.toml
  wezterm     a wt_hub_keys Lua table to merge into config.keys`;

/**
 * `wt hub` — launch (or re-attach to) the hub layout: an outer tmux
 * server hosting the task-inbox pane on the left and a live harness
 * pane on the right. See docs/hub.md. Also the target of a bare `wt`
 * when `[ui] mode = "hub"` is configured.
 *
 * `wt hub keys <terminal>` is a separate, non-interactive path: it
 * prints the command-layer generator output for the given terminal and
 * exits, rather than launching anything.
 */
export async function run(argv: string[]): Promise<number> {
  if (argv[0] === "keys") {
    return runKeys(argv.slice(1));
  }

  const { isHubPane, launchHub } = await import("../../core/hub.ts");
  if (isHubPane()) {
    console.error("already inside the hub — this command attaches from a plain terminal");
    return 2;
  }
  return launchHub();
}

async function runKeys(argv: string[]): Promise<number> {
  const terminal = argv[0];
  if (terminal === "alacritty") {
    const { renderAlacrittyBindings } = await import("../../core/hub.ts");
    console.log(renderAlacrittyBindings());
    return 0;
  }
  if (terminal === "wezterm") {
    const { renderWezTermBindings } = await import("../../core/hub.ts");
    console.log(renderWezTermBindings());
    return 0;
  }
  console.error(KEYS_USAGE);
  return 1;
}
