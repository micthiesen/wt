// Nord-ish palette. Keep the surface small so panels feel coherent.
export const theme = {
  bg: "#1b1d23",
  bgAlt: "#23262e",
  rowSelectedBg: "#3b4252",
  border: "#3b4252",
  borderDim: "#2e3440",
  fg: "#d8dee9",
  fgDim: "#747b8a",
  fgBright: "#eceff4",
  accent: "#88c0d0",
  accentAlt: "#81a1c1",
  // Muted teal: "working, but backgrounded". Same cool family as accent
  // (cyan) but quieter, so the `polling` AI-session state reads as
  // work-in-flight without the success connotation of green (`ok`).
  teal: "#6a9b8e",
  ok: "#a3be8c",
  warn: "#ebcb8b",
  err: "#bf616a",
  info: "#b48ead",
  // Anthropic / Claude brand orange. Mirrors CLAUDE_COLOR in
  // core/harness/claude.ts (core can't import this tui-layer palette).
  claude: "#c47b3a",
  // Codex brand indigo. `codex` mirrors CODEX_COLOR in
  // core/harness/codex.ts and lights the attention `waiting` state (the
  // brand color is the punchy "your turn" cue, mirroring claude's
  // orange). `codexAlt` is amber — the indigo complement — so `working`
  // contrasts in hue rather than just brightness, the way claude's cyan
  // working contrasts its orange waiting.
  codex: "#4d56d6",
  codexAlt: "#e0a94f",
  // OpenCode brand violet. `opencode` mirrors OPENCODE_COLOR in
  // core/harness/opencode.ts and lights `waiting`. `opencodeAlt` is
  // lime-green — the violet complement — so `working` reads as a
  // distinct hue, not a dimmed brand shade.
  opencode: "#a78bfa",
  opencodeAlt: "#9ccf6e",
};

/**
 * Re-skin the palette to the hub pane's static Catppuccin Mocha
 * palette (mirroring ~/.config/alacritty/alacritty.toml) — hub mode
 * only. This is NOT terminal-theme detection — the values below are
 * hardcoded to match the owner's Alacritty config, not read from the
 * terminal in any way. The hub's task pane lives INSIDE the terminal
 * next to a harness that renders on the terminal's own background, so
 * wt's Nord surface reads as a mismatched slab there; with `bg`/`bgAlt`
 * set to the same hardcoded background the pane blends in like Claude
 * Code does. Mutates the shared object in place BEFORE the first
 * render (theme values are read at render time) and only in the
 * `wt _taskpane` process — the classic TUI keeps Nord. Brand colors
 * (claude/codex/opencode) are identities, not theme, and stay put.
 */
export function applyHubPalette(): void {
  Object.assign(theme, {
    bg: "#1E1E2E", // base — identical to the terminal background
    bgAlt: "#1E1E2E", // bars blend into the terminal bg too
    rowSelectedBg: "#313244", // surface0
    border: "#45475A", // surface1
    borderDim: "#313244", // surface0
    fg: "#CDD6F4", // text
    fgDim: "#6C7086", // overlay0
    fgBright: "#F2F5FF",
    accent: "#89DCEB", // sky
    accentAlt: "#89B4FA", // blue
    teal: "#70A99C", // muted cyan (same "backgrounded work" role)
    ok: "#A6E3A1", // green
    warn: "#F9E2AF", // yellow
    err: "#F38BA8", // red
    info: "#CBA6F7", // mauve
  });
}

/**
 * Connector color for a stack row's parallel-lane index (`StackNode.lane`).
 * Lane 0 (the main spine, and every linear stack) stays dim; each forked
 * sibling lane picks a distinct hue from a small palette so the eye can
 * tell parallel branches apart without any extra indentation. The palette
 * deliberately avoids `ok`/`err` (status colors) so a lane tint never reads
 * as a state.
 */
const LANE_PALETTE = [theme.info, theme.teal, theme.accentAlt, theme.warn];
export function laneColor(lane: number): string {
  if (lane <= 0) return theme.fgDim;
  return LANE_PALETTE[(lane - 1) % LANE_PALETTE.length]!;
}
