// Nord-ish palette. Keep the surface small so panels feel coherent.
export const theme = {
  bg: "#1b1d23",
  bgAlt: "#23262e",
  rowSelectedBg: "#3b4252",
  /**
   * Subtler blue tint used to highlight every worktree in the current
   * row's stack chain while the `b` (stack) modal is open. Dim enough
   * to coexist with the selection bg on the cursor row without
   * competing, distinct enough that the chain reads as a group.
   */
  rowChainBg: "#2c3a4a",
  border: "#3b4252",
  borderDim: "#2e3440",
  fg: "#d8dee9",
  fgDim: "#747b8a",
  fgBright: "#eceff4",
  accent: "#88c0d0",
  accentAlt: "#81a1c1",
  ok: "#a3be8c",
  warn: "#ebcb8b",
  err: "#bf616a",
  info: "#b48ead",
  // Anthropic / Claude Code brand orange — used for the per-row
  // claude session count badge so the cluster reads as "claude".
  claudeOrange: "#d97757",
};
