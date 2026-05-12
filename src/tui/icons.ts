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

	// Review badges — shape varies by state so color isn't the sole
	// signal. Distinct family from `checkPass/Fail/Pend` (CI) so a row's
	// "did checks pass" and "did review pass" signals don't collide.
	thumbsUp: "\u{F164}", // nf-fa-thumbs_up    — review approved
	thumbsDown: "\u{F165}", // nf-fa-thumbs_down — review changes_requested
	hourglass: "\u{F43A}", // review pending (active)
	eye: "\u{F441}", // nf-oct-eye               — review unrequested ("eyes wanted")

	// CodeRabbit badge — single whimsy glyph (carrot, on-theme with the
	// "carrots / grazing / resting" vocab in pr.tsx); state is conveyed
	// by color since the carrot has no clean state-specific variants.
	// This is the deliberate "color-only" exception called out in
	// `badges.ts` rule #1's "if possible" caveat.
	carrot: "\u{EF3B}", // nf-md-carrot

	// Other badges
	mergeQueue: "\u{F4DB}", // nf-oct-git_merge_queue — Graphite mergeability badge
	bolt: "\u{F0E7}", // nf-fa-bolt              — SST stage deployed
	boltOff: "\u{F05E}", // nf-fa-ban            — SST stage not deployed (no entry)
	comment: "\u{F41F}", // nf-oct-comment        — claude action-running marker (list + details)
	// Filled circled digits for the per-row claude session count.
	// Unicode dingbats (U+2776..U+277E for 1..9) — guaranteed to render
	// in any monospace font, no NerdFont dependency. Matches the visual
	// weight of the comment glyph being replaced.
	circled1: "\u{2776}",
	circled2: "\u{2777}",
	circled3: "\u{2778}",
	circled4: "\u{2779}",
	circled5: "\u{277A}",
	circled6: "\u{277B}",
	circled7: "\u{277C}",
	circled8: "\u{277D}",
	circled9: "\u{277E}",
	anglesUp: "\u{F102}", // nf-fa-angle_double_up — stacked-on hint when parent worktree sits immediately above
	pin: "\u{F08D}", // nf-fa-thumb_tack — pinned output in OutputViewer title
} as const;

/**
 * Glyph for a per-row claude session count. 1..9 → filled circled
 * digit; 10+ → "9+" sigil (filled circled 9 followed by a plus). 0
 * shouldn't reach here — callers should omit the badge entirely.
 */
export function claudeCountGlyph(count: number): string {
  if (count <= 0) return "";
  if (count === 1) return NF.circled1;
  if (count === 2) return NF.circled2;
  if (count === 3) return NF.circled3;
  if (count === 4) return NF.circled4;
  if (count === 5) return NF.circled5;
  if (count === 6) return NF.circled6;
  if (count === 7) return NF.circled7;
  if (count === 8) return NF.circled8;
  if (count === 9) return NF.circled9;
  return `${NF.circled9}+`;
}
