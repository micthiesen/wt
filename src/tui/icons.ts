/**
 * Nerd Font icons used across the TUI. Requires a Nerd-Font-patched
 * terminal font (the setup script installs `Lilex Nerd Font Mono`).
 * Keeping every PUA codepoint in one table means call sites can use
 * readable names (`NF.rocket`) without littering source files with
 * unrenderable glyphs.
 *
 * Codepoint references: https://www.nerdfonts.com/cheat-sheet
 * All glyphs are 1-cell wide (the font was patched with `--mono`).
 */
export const NF = {
  // Row status markers
  rocket: "\u{F427}", // nf-oct-rocket          — busy, non-destructive op
  trash: "\u{F48E}", // nf-oct-trash           — busy, destructive (rm)
  unlink: "\u{F529}", // nf-oct-unlink         — worktree path vanished
  slash: "\u{F468}", // nf-oct-circle_slash    — branch gone from remote
  merge: "\u{F419}", // nf-oct-git_merge       — branch merged into main
  pencil: "\u{F448}", // nf-oct-pencil         — uncommitted changes
  clean: "\u{F06C}", // nf-fa-leaf             — clean working tree, at rest

  // PR badges
  prOpen: "\u{F407}", // nf-oct-git_pull_request
  prDraft: "\u{F4DD}", // nf-oct-git_pull_request_draft
  prMerged: "\u{F4C9}", // nf-oct-feed_merged (distinct from status `merge`)
  prClosed: "\u{F4DC}", // nf-oct-git_pull_request_closed

  // CI check badges
  checkPass: "\u{F49E}", // nf-oct-check_circle
  checkFail: "\u{F52F}", // nf-oct-x_circle
  checkPend: "\u{F43A}", // nf-oct-clock

  // Other badges
  mergeQueue: "\u{F4DB}", // nf-oct-git_merge_queue — prefix for `⇥N`
  bolt: "\u{F0E7}", // nf-fa-bolt              — SST stage deployed
  boltOff: "\u{F05E}", // nf-fa-ban            — SST stage not deployed (no entry)
} as const;
