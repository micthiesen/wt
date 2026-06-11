/**
 * Shared renderer handoff for F10/F11/F12: suspend the opentui
 * renderer, hand the terminal to a child (tmux attach) for the
 * duration of `fn`, resume on the way back.
 *
 * The SIGWINCH detach is the load-bearing part. opentui's `suspend()`
 * leaves its resize listener installed, and `processResize` fires a
 * pixel-resolution query (`CSI 14 t`) even while suspended — twice,
 * in fact. With tmux owning the terminal, the terminal's `CSI 4;h;w t`
 * reply lands in the tmux client's stdin as unrecognized key input and
 * is forwarded verbatim to the active pane: the "4;1360;1332t" garbage
 * typed into a Claude session whenever the window resizes during an
 * attach (e.g. the hyper+t Alacritty size toggle). Detaching every
 * SIGWINCH listener for the handoff and synthesizing one SIGWINCH
 * after resume keeps the query/reply cycle confined to windows where
 * the renderer actually owns stdin. The resync is free when the size
 * didn't change (`processResize` no-ops on equal dimensions).
 */
import type { CliRenderer } from "@opentui/core";

/**
 * Clear-screen + cursor-home. opentui's suspend emits `\x1b[?1049l`
 * which drops the terminal back to its main screen, briefly exposing
 * whatever was there before wt started. Painting a clean screen into
 * that brief window replaces the pre-wt scroll with a uniform black
 * gap so the transition reads as a clean cut rather than a flash of
 * unrelated content. Symmetric on resume so the same window on the
 * way back doesn't reveal the inner program's last frame either.
 */
const CLEAR_SCREEN = "\x1b[2J\x1b[H";

export async function handoffTerminal<T>(
  renderer: CliRenderer,
  fn: () => Promise<T>,
): Promise<T> {
  renderer.suspend();
  process.stdout.write(CLEAR_SCREEN);
  const winchListeners = process.listeners("SIGWINCH");
  for (const l of winchListeners) {
    process.removeListener("SIGWINCH", l as NodeJS.SignalsListener);
  }
  try {
    return await fn();
  } finally {
    for (const l of winchListeners) {
      process.on("SIGWINCH", l as NodeJS.SignalsListener);
    }
    process.stdout.write(CLEAR_SCREEN);
    renderer.resume();
    // Catch up on any resize that happened while the listeners were
    // detached — the handler reads current stdout.columns/rows, so one
    // synthetic signal fully resyncs the layout.
    process.kill(process.pid, "SIGWINCH");
  }
}
