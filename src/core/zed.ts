import { run } from "./proc.ts";

import {
  findZedWindowForPath,
  focusYabaiWindow,
  spawnZedAndTrack,
} from "./zed-windows.ts";

/**
 * If Alacritty is frontmost, hide it — same visual effect as Cmd+H.
 * No-op from other terminals. Best-effort; any error (missing osascript,
 * no automation perms, sandboxed terminal) is swallowed because this is
 * purely cosmetic UX.
 *
 * Hides via a process-property write (`set visible ... to false`) rather
 * than a synthetic Cmd+H keystroke. Sending keystrokes needs the stricter
 * Accessibility TCC permission, which a macOS or Alacritty update can
 * silently reset — leaving the frontmost read working (that only needs
 * Automation) while the keystroke fails with "not allowed to send
 * keystrokes (1002)", which the catch swallowed, so the hide quietly
 * no-oped. Setting `visible` needs only Automation, the same bucket the
 * frontmost query already relies on, so the two can't drift apart.
 *
 * One osascript call does both the frontmost check and the hide, closing
 * the window where focus could change between two separate invocations.
 * `ignoring case` covers osascript returning either `alacritty` or the
 * marketing-name `Alacritty` across macOS versions.
 */
export async function hideFrontmostAlacritty(): Promise<void> {
  try {
    await run([
      "osascript",
      "-e", 'tell application "System Events"',
      "-e", "set p to first application process whose frontmost is true",
      "-e", "ignoring case",
      "-e", 'if name of p is "alacritty" then set visible of p to false',
      "-e", "end ignoring",
      "-e", "end tell",
    ]);
  } catch (err) {
    void err;
  }
}

/**
 * Open `path` in Zed using focus-if-open, else-new-window semantics.
 * Zed 0.20x made `zed <path>` reuse the current window regardless of
 * whether another window already has the path open, so we track each
 * spawn's yabai window id in `~/.cache/wt/zed-windows.json` and focus
 * via yabai when one exists. Unified helper — both CLI and TUI call
 * this so behavior stays in sync.
 *
 * Returns after the spawn is either focused or tracking has been
 * recorded. Awaiting matters for short-lived CLI callers: the parent
 * exits right after and a background tracking poll wouldn't survive
 * `process.exit`.
 */
export async function openInZed(path: string): Promise<void> {
  await hideFrontmostAlacritty();
  const existing = await findZedWindowForPath(path);
  if (existing !== null && (await focusYabaiWindow(existing))) return;
  await spawnZedAndTrack(path);
}
