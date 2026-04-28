import {
  findZedWindowForPath,
  focusYabaiWindow,
  spawnZedAndTrack,
} from "./zed-windows.ts";

/**
 * If Alacritty is frontmost, send Cmd+H so the window hides — matches
 * the manual shortcut. No-op from other terminals. Best-effort; any
 * error (missing osascript, no accessibility perms, sandboxed
 * terminal) is swallowed because this is purely cosmetic UX.
 */
export function hideFrontmostAlacritty(): void {
  try {
    const frontmostProc = Bun.spawnSync([
      "osascript",
      "-e",
      'tell application "System Events" to name of first application process whose frontmost is true',
    ]);
    // osascript on some macOS versions returns the lowercased process
    // name (`alacritty`) and on others returns the marketing name
    // (`Alacritty`). Compare case-insensitively.
    const frontmost = frontmostProc.stdout.toString().trim().toLowerCase();
    if (frontmost !== "alacritty") return;
    Bun.spawnSync([
      "osascript",
      "-e",
      'tell application "System Events" to key code 4 using {command down}',
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
  hideFrontmostAlacritty();
  const existing = findZedWindowForPath(path);
  if (existing !== null && focusYabaiWindow(existing)) return;
  await spawnZedAndTrack(path);
}
