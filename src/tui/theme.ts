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
  // orange); `codexAlt` is a calmer lighter indigo for `working`.
  codex: "#4d56d6",
  codexAlt: "#9aa0ee",
  // OpenCode brand violet. `opencode` mirrors OPENCODE_COLOR in
  // core/harness/opencode.ts and lights `waiting`; `opencodeAlt` is a
  // muted violet for the calmer `working` state.
  opencode: "#a78bfa",
  opencodeAlt: "#7d6bb5",
};
