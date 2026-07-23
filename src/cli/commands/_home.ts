/**
 * `wt _home` — internal: the static dashboard the hub's right pane
 * shows when the selected task has no live session. Runs inside the
 * reserved `wt-hub-home` session on the inner tmux server. Prints a
 * key legend once and sleeps; wt switch-clients away from it the
 * moment a task with a live session is selected.
 */
const DIM = "\x1b[2m";
const BOLD = "\x1b[1m";
const RESET = "\x1b[0m";

function line(key: string, desc: string): string {
  return `  ${BOLD}${key.padEnd(12)}${RESET}${DIM}${desc}${RESET}`;
}

export async function run(_argv: string[]): Promise<number> {
  // Clear + home; keep the output minimal and legible at any pane size.
  process.stdout.write("\x1b[2J\x1b[H");
  const out = [
    "",
    `  ${BOLD}wt hub${RESET}`,
    `  ${DIM}no live session for the selected task${RESET}`,
    "",
    line("Enter / F12", "start + show the task's agent session"),
    line("F11 / F10", "show the diff / shell session"),
    line("M-j / M-k", "move through the task inbox"),
    line("M-Space", "zoom this pane"),
    line("F9", "switch pane focus"),
    line("M-?", "full keymap"),
    "",
  ].join("\n");
  process.stdout.write(out + "\n");
  // Sleep forever — tmux owns this pane's lifetime; wt just switches
  // the client away. Ctrl+C ends the process (and the session), which
  // is fine: showHome() re-ensures it on demand.
  await new Promise<void>(() => {});
  return 0;
}
