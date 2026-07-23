/**
 * Single source of truth for hub mode's "command layer" — the table of
 * cmd+<key> chords that a terminal must translate into ESC-prefixed byte
 * sequences so the outer hub tmux server (`buildHubConfig` in
 * `./config.ts`) can pick them up as `M-<key>` root-table bindings and
 * relay them into the task pane. See docs/hub.md#the-command-layer for
 * the conceptual writeup.
 *
 * This module owns two things:
 *   1. `COMMAND_LAYER` — the chord table itself, terminal-agnostic.
 *   2. `renderAlacrittyBindings()` / `renderWezTermBindings()` — pure
 *      functions that format that same table for each terminal's config
 *      syntax. `wt hub keys <terminal>` (`cli/commands/hub.ts`) just
 *      prints whichever renderer's output.
 *
 * Historically this only existed as a hand-maintained block in the
 * owner's Alacritty config (`~/.dotfiles/alacritty/.config/alacritty/
 * alacritty.toml`). Keeping the chord list here — instead of in either
 * renderer — means a new/changed binding is a one-line table edit that
 * both renderers (and the drift-guard test in `config.test.ts`) pick up
 * automatically.
 */

/** A macOS keyboard chord: `cmd+<key>` or `cmd+shift+<key>`. */
export type Chord = {
  /**
   * The physical key, lowercased, as it would be typed unmodified:
   * a letter (`"j"`), a digit (`"1"`), punctuation (`";"`, `"."`,
   * `"/"`), or the named key `"Backspace"`.
   */
  readonly key: string;
  /** True for `cmd+shift+<key>` chords (only `cmd+shift+m` today). */
  readonly shift: boolean;
};

/** One row of the command layer: a chord, what it sends, and why. */
export type CommandLayerEntry = {
  readonly chord: Chord;
  /**
   * The raw bytes the terminal must send AFTER the ESC (`\x1b`) prefix
   * every chord shares. Most are the bare key itself; a few are control
   * characters (`"\r"`, `"\t"`, `"\x7f"`) because the wt keybinding they
   * trigger doesn't share the chord's literal letter (e.g. cmd+l opens
   * a session the same way pressing Enter would classically).
   */
  readonly bytes: string;
  /**
   * The outer tmux server's root-table key name for this binding, e.g.
   * `"M-j"`, `"M-Enter"`, `"M-BSpace"`. Derived from `bytes` via
   * `bytesToTmuxKeyName` rather than hand-written, so the chord table
   * and the tmux-facing name can't drift apart from a typo.
   */
  readonly tmuxKey: string;
  /** Short human label — used in generator comments and docs. */
  readonly label: string;
};

/**
 * Map a chord's post-ESC bytes to the tmux root-table key name that
 * receiving those bytes (as `ESC <bytes>`) produces. tmux names most
 * Meta-prefixed keys after the raw byte (`M-j`, `M-;`), but a handful of
 * control characters get their own mnemonic name instead of the literal
 * control byte — those three are hand-cased below; everything else
 * (single printable characters) maps to `M-<byte>` directly.
 */
export function bytesToTmuxKeyName(bytes: string): string {
  switch (bytes) {
    case "\r":
      return "M-Enter";
    case "\t":
      return "M-Tab";
    case "\x7f":
      return "M-BSpace";
    default:
      return `M-${bytes}`;
  }
}

function entry(key: string, bytes: string, label: string, shift = false): CommandLayerEntry {
  return { chord: { key, shift }, bytes, tmuxKey: bytesToTmuxKeyName(bytes), label };
}

/**
 * The authoritative chord table. Order here is the order both renderers
 * emit bindings in, and the order docs/generator comments walk through
 * them.
 */
export const COMMAND_LAYER: readonly CommandLayerEntry[] = [
  entry("j", "j", "task down"),
  entry("k", "k", "task up"),
  entry("l", "\r", "open/show session"),
  entry("u", "u", "focus task pane"),
  entry("1", "1", "jump to task 1..9"),
  entry("2", "2", ""),
  entry("3", "3", ""),
  entry("4", "4", ""),
  entry("5", "5", ""),
  entry("6", "6", ""),
  entry("7", "7", ""),
  entry("8", "8", ""),
  entry("9", "9", ""),
  entry("d", "d", "diff view"),
  entry("s", "s", "shell view"),
  entry("t", "t", "new worktree"),
  entry("e", "\t", "expand/collapse"),
  entry("z", "z", "snooze"),
  entry("p", "P", "pin"),
  entry("i", "I", "details card"),
  entry("o", "p", "open PR"),
  entry("r", "r", "refresh"),
  entry("m", "M", "merge toggle", true),
  entry("f", "f", "zoom session pane"),
  entry("w", "w", "close session"),
  entry(";", ";", "sessions picker"),
  entry(".", ".", "action picker"),
  entry("/", "/", "help"),
  entry("Backspace", "\x7f", "destroy worktree"),
] as const;

/** Chords deliberately left unbound, and why — shared by both renderers' comment headers. */
export const UNBOUND_NOTE = [
  "cmd+n is left untouched — it stays the terminal's own new-window/new-tab chord.",
  "cmd+h and cmd+m cannot be bound at all: macOS's menu bar consumes",
  "Hide/Minimize before any terminal sees them. That's why focus-tasks",
  "rides cmd+u instead of cmd+h, and merge rides cmd+shift+m instead of cmd+m.",
] as const;

/** Known conflicts with terminal defaults — shared by both renderers' comment headers. */
export const KNOWN_COSTS_NOTE = [
  "cmd+k shadows clear-scrollback and cmd+f shadows search in the terminal itself.",
] as const;

/** Optional zsh zle-widget snippet note, appended as a comment by both renderers. */
export const ZSH_WIDGET_NOTE = [
  "Optional: at a bare zsh prompt (outside the hub), cmd+w's ESC-prefixed",
  "sequence does nothing by default. To make it close the window the way",
  "closing a session does inside the hub, add to ~/.zshrc:",
  "",
  "  _wt_hub_close_shell() { exit }",
  "  zle -N _wt_hub_close_shell",
  "  bindkey '\\ew' _wt_hub_close_shell",
] as const;

function alacrittyKeyName(key: string): string {
  if (key === "Backspace") return "Back";
  if (key === ";") return "Semicolon";
  if (key === ".") return "Period";
  if (key === "/") return "Slash";
  if (/^[0-9]$/.test(key)) return `Key${key}`;
  return key.toUpperCase();
}

/** Escape a chord's post-ESC bytes for an Alacritty TOML `chars = "..."` value. */
function alacrittyChars(bytes: string): string {
  const escaped = bytes === "\r"
    ? "\\r"
    : bytes === "\t"
    ? "\\t"
    : bytes === "\x7f"
    ? "\\u007f"
    : bytes;
  return `\\u001b${escaped}`;
}

/**
 * Render the ready-to-paste `[keyboard] bindings` entries for Alacritty.
 * Pure formatting over `COMMAND_LAYER` — no logic here decides which
 * chords exist or what they mean, only how to spell them in Alacritty's
 * TOML binding syntax.
 */
export function renderAlacrittyBindings(): string {
  const header = [
    "# ── wt hub command layer ─────────────────────────────────────────────",
    "# cmd+<key> -> ESC-prefixed sequence -> the wt hub's outer tmux server",
    "# sees it as M-<key> and drives the task pane, regardless of which pane",
    "# holds focus. Bindings are UNCONDITIONAL (no `mode` scoping) — outside",
    "# the hub they degrade to harmless Meta keystrokes.",
    "#",
    ...UNBOUND_NOTE.map((l) => `# ${l}`),
    "#",
    ...KNOWN_COSTS_NOTE.map((l) => `# ${l}`),
    "#",
    ...ZSH_WIDGET_NOTE.map((l) => (l ? `# ${l}` : "#")),
  ].join("\n");

  const lines = COMMAND_LAYER.map((e) => {
    const key = alacrittyKeyName(e.chord.key);
    const mods = e.chord.shift ? "Command|Shift" : "Command";
    const chars = alacrittyChars(e.bytes);
    const line = `  { key = "${key}", mods = "${mods}", chars = "${chars}" },`;
    return e.label ? `${line}  # ${e.label}` : line;
  }).join("\n");

  return `${header}\n[keyboard]\nbindings = [\n${lines}\n]\n`;
}

/** Escape a chord's post-ESC bytes for a WezTerm Lua single-quoted string. */
function wezTermChars(bytes: string): string {
  const escaped = bytes === "\r"
    ? "\\r"
    : bytes === "\t"
    ? "\\t"
    : bytes === "\x7f"
    ? "\\x7f"
    : bytes;
  return `'\\x1b${escaped}'`;
}

function wezTermKeyName(key: string): string {
  return key === "Backspace" ? "Backspace" : key;
}

/**
 * Render the ready-to-paste `wt_hub_keys` Lua table for WezTerm. Pure
 * formatting over `COMMAND_LAYER`, same shared source as the Alacritty
 * renderer.
 */
export function renderWezTermBindings(): string {
  const header = [
    "-- ── wt hub command layer ─────────────────────────────────────────────",
    "-- cmd+<key> -> ESC-prefixed SendString -> the wt hub's outer tmux server",
    "-- sees it as M-<key> and drives the task pane, regardless of which pane",
    "-- holds focus.",
    "--",
    "-- These entries SHADOW WezTerm's own defaults for the same chords:",
    "--   CMD+t SpawnTab, CMD+w CloseCurrentTab, CMD+k ClearScrollback,",
    "--   CMD+f Search, CMD+1..CMD+9 ActivateTab(0..8) — rehome tab",
    "--   management onto other chords if you rely on them.",
    "--",
    ...UNBOUND_NOTE.map((l) => `-- ${l}`),
    "--",
    ...ZSH_WIDGET_NOTE.map((l) => (l ? `-- ${l}` : "--")),
    "--",
    "-- Merge into your wezterm config, e.g. (assuming `local wezterm =",
    "-- require 'wezterm'` and `local config = wezterm.config_builder()`",
    "-- already exist further up your config file):",
    "--   config.keys = wt_hub_keys",
    "--   -- or, to keep your existing keys:",
    "--   -- for _, k in ipairs(wt_hub_keys) do table.insert(config.keys, k) end",
  ].join("\n");

  const lines = COMMAND_LAYER.map((e) => {
    const key = wezTermKeyName(e.chord.key);
    const mods = e.chord.shift ? "CMD|SHIFT" : "CMD";
    const chars = wezTermChars(e.bytes);
    const line =
      `  { key = '${key}', mods = '${mods}', action = wezterm.action.SendString ${chars} },`;
    return e.label ? `${line} -- ${e.label}` : line;
  }).join("\n");

  return `${header}\nlocal wt_hub_keys = {\n${lines}\n}\n`;
}
