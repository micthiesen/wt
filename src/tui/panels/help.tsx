import type { ReactNode } from "react";

import { getHarness } from "../../core/harness/index.ts";
import { STATE_DOT, STATE_FG } from "../claude-state.ts";
import { NF } from "../icons.ts";
import { Modal } from "../modal.tsx";
import type { KeyHintPair } from "../key-hint.tsx";
import { theme } from "../theme.ts";

type LegendGlyph = ReactNode;

/** A single `key → meaning` line in a keybinding block. */
type KeyItem = { key: string; label: string };

/** A single `glyph → meaning` line in a visual-reference block. */
type GlyphItem = {
  glyph: LegendGlyph;
  color: string;
  label: string;
  /** Text used for `/` search matching when `glyph` is a node (nerd-font
   *  glyphs and colored spans aren't typeable). When omitted, matching
   *  falls back to the string glyph (if `glyph` is a string) plus the
   *  label; a node glyph then contributes nothing, so node glyphs that
   *  need to be findable beyond their label should set this. */
  search?: string;
};

/**
 * Both columns are built from the same `Block` union so the `/` filter
 * can walk one list. `keys` blocks render with `KeyRow` (left column,
 * the keymap); `glyphs` blocks render with `GlyphRow` (right column,
 * the visual reference).
 */
type Block =
  | { kind: "keys"; title: string; note?: string; items: KeyItem[] }
  | {
      kind: "glyphs";
      title: string;
      note?: string;
      /** Width reserved for the glyph column; widen for multi-cell notation. */
      glyphWidth?: number;
      items: GlyphItem[];
    };

/** Width of the key column. Sized to the widest common key
 *  (`ctrl+j / ctrl+k`, 15 cells) plus a one-cell gutter; anything longer
 *  wraps within the column (wrapMode) rather than overrunning the label. */
const KEY_W = 16;

// ── Left column: the keymap, grouped by what you're acting on ──────────

const KEY_BLOCKS: Block[] = [
  {
    kind: "keys",
    title: "navigation",
    items: [
      { key: "j / ↓", label: "down" },
      { key: "k / ↑", label: "up" },
      { key: "g", label: "top" },
      { key: "G", label: "bottom" },
      { key: "TAB", label: "fold / unfold the section under the cursor (Inbox too)" },
      { key: "ctrl+j / ctrl+k", label: "scroll details pane" },
    ],
  },
  {
    kind: "keys",
    title: "worktree",
    items: [
      { key: "o", label: "open in zed" },
      { key: "p", label: "open PR in browser" },
      { key: "i", label: "open linear issue" },
      { key: "s", label: "open deployed app" },
      { key: "y", label: "yank menu (b/s/S/p/n/i/r)" },
      { key: "t", label: "regenerate AI summary" },
      { key: "a", label: "archive / restore" },
      { key: "d", label: "remove worktree" },
    ],
  },
  {
    kind: "keys",
    title: "pull request",
    items: [
      { key: "m", label: "merge when ready (toggle auto-merge)" },
      { key: "e", label: "exit draft (mark ready for review)" },
      { key: "E", label: "ship: mark ready + request default_reviewer + arm auto-merge" },
      { key: "v", label: "edit reviewers (picker · v v submits)" },
    ],
  },
  {
    kind: "keys",
    title: "sessions",
    items: [
      { key: "!", label: "run claude action · kill if running" },
      { key: "! c", label: "open action picker, jump to custom prompt" },
      { key: ";", label: "sessions picker (all harnesses for current row)" },
      { key: "; c / x / o", label: "jump to + new claude / codex / opencode" },
      { key: "; d", label: "close highlighted session gracefully (ctrl+d ×2)" },
      { key: "; x", label: "kill highlighted session" },
      { key: "⇧TAB", label: "cycle primary harness (top-right)" },
      { key: "⇧F12", label: "harness picker (one-off spawn)" },
      { key: "F10", label: "enter shell · F10 again to detach" },
      { key: "⇧F10", label: "kill shell session (ends background procs)" },
      { key: "F11", label: "enter diff TUI · F11 again to detach" },
      { key: "⇧F11", label: "kill diff session (resets gitu state)" },
      { key: "F12", label: "enter F12-target session · F12 again to detach" },
      { key: "ctrl+d", label: "close F12-target session gracefully (ctrl+d ×2)" },
    ],
  },
  {
    kind: "keys",
    title: "organize",
    items: [
      { key: "l", label: "set section (picker) · manual sections only" },
      { key: "l l", label: "confirm highlighted section" },
      { key: "l n", label: "new section (chord)" },
      { key: "L", label: "rename current section" },
      { key: "b", label: "set fork base (picker · b b confirms) · record only, never rebases" },
      { key: "J / K", label: "move row · stack/folded section: move whole group" },
    ],
  },
  {
    kind: "keys",
    title: "global",
    items: [
      { key: "n", label: "new worktree" },
      { key: "N", label: "new worktree · base = selected" },
      { key: "c", label: "clean merged/gone" },
      { key: "r", label: "refresh (fetch + recompute)" },
      { key: "R", label: "restack this stack (replay; /restack on conflict)" },
      { key: "^R", label: "clear all cached data" },
      { key: ",", label: "enter wt source session · F12 to detach" },
      { key: ".", label: "enter main repo session · F12 to detach" },
      { key: "/", label: "enter dotfiles session · F12 to detach" },
      { key: ">", label: "open wt source in zed" },
      { key: "O", label: "open main repo in zed" },
      { key: "?", label: "toggle this help" },
      { key: "q / ^C", label: "quit" },
    ],
  },
  {
    kind: "keys",
    title: "review requests (pinned section)",
    note: "PRs awaiting your review, pulled from GitHub. Not worktrees — only these keys apply.",
    items: [
      { key: "p / ⏎", label: "open PR on GitHub" },
      { key: "w", label: "check out branch as worktree → Reviews" },
    ],
  },
  {
    kind: "keys",
    title: "outputs (bottom pane · per-worktree)",
    note: "each worktree remembers its last-shown output; switching rows restores it.",
    items: [
      { key: "'", label: "outputs picker for this worktree" },
      { key: "' '", label: "confirm highlighted output (chord)" },
      { key: "[ / ]", label: "cycle prev / next output for this worktree" },
      { key: '"', label: "jump to events for this worktree" },
      { key: "esc", label: "clear focus (return to follow-row auto)" },
    ],
  },
  {
    kind: "keys",
    title: "modals",
    note: "every list picker follows the same trigger-key-confirm pattern. esc cancels; j/k or arrows move; the bottom pane previews the highlight when applicable. CLAUDE.md has the full rules.",
    items: [
      { key: "key", label: "open picker (l, ;, ', !, v …)" },
      { key: "key key", label: "confirm highlighted (trigger re-press)" },
      { key: "⏎", label: "confirm highlighted" },
      { key: "esc / q", label: "cancel" },
      { key: "1-9", label: "quick pick by row digit" },
    ],
  },
];

// ── Right column: the visual reference ─────────────────────────────────

const STATUS_GLYPHS: GlyphItem[] = [
  { glyph: NF.rocket, color: theme.accent, label: "busy (init/install)" },
  { glyph: NF.trash, color: theme.err, label: "busy (removing)" },
  { glyph: NF.unlink, color: theme.err, label: "missing (path vanished)" },
  { glyph: NF.slash, color: theme.warn, label: "gone (branch deleted upstream)" },
  { glyph: NF.merge, color: theme.ok, label: "merged into origin/main" },
  { glyph: NF.pencil, color: theme.warn, label: "uncommitted changes" },
  { glyph: "  ", color: theme.fgDim, label: "idle / clean", search: "idle clean" },
];

const BADGES: GlyphItem[] = [
  { glyph: NF.prOpen, color: theme.accentAlt, label: "PR open" },
  { glyph: NF.prDraft, color: theme.fgDim, label: "PR draft" },
  { glyph: NF.prMerged, color: theme.info, label: "PR merged" },
  { glyph: NF.prClosed, color: theme.err, label: "PR closed" },
  { glyph: NF.checkPass, color: theme.ok, label: "CI checks passing" },
  { glyph: NF.checkFail, color: theme.err, label: "CI checks failing" },
  { glyph: NF.checkPend, color: theme.warn, label: "CI checks pending" },
  { glyph: `${NF.mergeQueue} N`, color: theme.ok, label: "merge queue pos N · mergeable" },
  { glyph: `${NF.mergeQueue} N`, color: theme.warn, label: "merge queue pos N · awaiting/queued" },
  { glyph: `${NF.mergeQueue} N`, color: theme.err, label: "merge queue pos N · blocked" },
  { glyph: NF.mergeQueue, color: theme.info, label: "auto-merge armed (waiting)" },
  { glyph: NF.bolt, color: theme.warn, label: "SST stage deployed" },
  { glyph: NF.comment, color: theme.ok, label: "Claude · `!` action running" },
  {
    glyph: getHarness("claude").glyph,
    color: getHarness("claude").color,
    label: "AI session live (list pane shows the F12-target harness glyph)",
    search: "ai session live harness",
  },
  { glyph: STATE_DOT.working, color: STATE_FG.working, label: "AI session · working", search: "ai session working" },
  { glyph: STATE_DOT.asking, color: STATE_FG.asking, label: "AI session · asking", search: "ai session asking" },
  { glyph: STATE_DOT.polling, color: STATE_FG.polling, label: "AI session · polling", search: "ai session polling" },
  { glyph: STATE_DOT.unknown, color: STATE_FG.unknown, label: "AI session · unknown", search: "ai session unknown" },
  { glyph: STATE_DOT.waiting, color: STATE_FG.waiting, label: "AI session · waiting", search: "ai session waiting" },
  { glyph: STATE_DOT.idle, color: STATE_FG.idle, label: "AI session · idle", search: "ai session idle" },
  { glyph: STATE_DOT.abandoned, color: STATE_FG.abandoned, label: "AI session · abandoned", search: "ai session abandoned" },
];

const SYNC: GlyphItem[] = [
  {
    glyph: <text fg={theme.fgDim}>(↑N ↓M)</text>,
    color: theme.fgDim,
    label: "ahead/behind vs remote branch",
    search: "(↑n ↓m) ahead behind remote branch",
  },
  {
    glyph: <text fg={theme.fgDim}>[↑N ↓M]</text>,
    color: theme.fgDim,
    label: "ahead/behind vs origin/main",
    search: "[↑n ↓m] ahead behind origin main",
  },
  {
    glyph: (
      <text>
        <span fg={theme.warn}>↑</span> <span fg={theme.err}>↓</span>
      </text>
    ),
    color: theme.fg,
    label: "↑ ahead · ↓ behind",
    search: "ahead behind up down arrows",
  },
];

const REFERENCE_BLOCKS: Block[] = [
  { kind: "glyphs", title: "status glyphs", items: STATUS_GLYPHS },
  { kind: "glyphs", title: "row badges", items: BADGES },
  { kind: "glyphs", title: "sync notation", glyphWidth: 8, items: SYNC },
  {
    kind: "keys",
    title: "new: prompt flags",
    items: [
      { key: "--any", label: "match any author" },
      { key: "--base <ref>", label: "branch off <ref>" },
    ],
  },
  {
    kind: "keys",
    title: "selection",
    items: [{ key: "mouse drag", label: "selects + auto-copies to clipboard" }],
  },
];

// ── Filtering ──────────────────────────────────────────────────────────

function glyphSearchText(it: GlyphItem): string {
  const g = it.search ?? (typeof it.glyph === "string" ? it.glyph : "");
  return `${g} ${it.label}`.toLowerCase();
}

/** Filter a block to rows matching `q` (lowercased, non-empty). A title
 *  hit keeps every row; otherwise only matching rows survive. Returns
 *  null when nothing in the block matches. */
function filterBlock(block: Block, q: string): Block | null {
  const titleHit = block.title.toLowerCase().includes(q);
  if (block.kind === "keys") {
    const items = block.items.filter(
      (it) => titleHit || `${it.key} ${it.label}`.toLowerCase().includes(q),
    );
    return items.length ? { ...block, items } : null;
  }
  const items = block.items.filter((it) => titleHit || glyphSearchText(it).includes(q));
  return items.length ? { ...block, items } : null;
}

function filterColumn(blocks: Block[], q: string): Block[] {
  if (!q) return blocks;
  return blocks
    .map((b) => filterBlock(b, q))
    .filter((b): b is Block => b !== null);
}

function countRows(blocks: Block[]): number {
  return blocks.reduce((n, b) => n + b.items.length, 0);
}

// ── Row renderers ──────────────────────────────────────────────────────

function KeyRow({ keyText, label }: { keyText: string; label: string }) {
  return (
    <box flexDirection="row">
      <box width={KEY_W} flexShrink={0}>
        <text fg={theme.accent} attributes={1} wrapMode="word">
          {keyText}
        </text>
      </box>
      <box flexGrow={1} flexShrink={1}>
        <text fg={theme.fg} wrapMode="word">
          {label}
        </text>
      </box>
    </box>
  );
}

function GlyphRow({
  glyph,
  color,
  label,
  width,
}: {
  glyph: LegendGlyph;
  color: string;
  label: string;
  width: number;
}) {
  return (
    <box flexDirection="row">
      <box width={width} flexShrink={0}>
        {typeof glyph === "string" ? <text fg={color}>{glyph}</text> : glyph}
      </box>
      <box flexGrow={1} flexShrink={1}>
        <text fg={theme.fg} wrapMode="word">
          {label}
        </text>
      </box>
    </box>
  );
}

function BlockView({ block }: { block: Block }) {
  return (
    <box flexDirection="column" marginBottom={1}>
      <text fg={theme.fgDim} attributes={1}>
        {block.title}
      </text>
      {block.note ? (
        <text fg={theme.fgDim} wrapMode="word">
          {block.note}
        </text>
      ) : null}
      {block.kind === "keys"
        ? block.items.map((it) => (
            <KeyRow key={it.key} keyText={it.key} label={it.label} />
          ))
        : block.items.map((it) => (
            <GlyphRow
              key={it.label}
              glyph={it.glyph}
              color={it.color}
              label={it.label}
              width={block.glyphWidth ?? 5}
            />
          ))}
    </box>
  );
}

function Column({ blocks }: { blocks: Block[] }) {
  return (
    <box flexDirection="column" flexGrow={1} flexBasis={0}>
      {blocks.map((b) => (
        <BlockView key={b.title} block={b} />
      ))}
    </box>
  );
}

// ── Overlay ────────────────────────────────────────────────────────────

export function HelpOverlay({
  query,
  searching,
}: {
  query: string;
  searching: boolean;
}) {
  const q = query.trim().toLowerCase();
  const left = filterColumn(KEY_BLOCKS, q);
  const right = filterColumn(REFERENCE_BLOCKS, q);
  const matches = countRows(left) + countRows(right);
  const empty = q.length > 0 && matches === 0;

  const hints: KeyHintPair[] = searching
    ? [
        ["type", "filter"],
        ["⏎", "apply"],
        ["esc", "cancel"],
      ]
    : query
      ? [
          // Nothing to scroll when the filter matches nothing.
          ...(empty ? [] : ([["j k", "scroll"]] as KeyHintPair[])),
          ["/", "search"],
          ["esc", "clear"],
          ["? / q", "close"],
        ]
      : [
          ["j k / ↑ ↓", "scroll"],
          ["/", "search"],
          ["? / esc / q", "close"],
        ];

  return (
    <Modal
      title="help"
      inset={{ top: "6%", right: "6%", bottom: "6%", left: "6%" }}
      hints={hints}
    >
      {searching || query ? (
        <box flexShrink={0} flexDirection="row" marginBottom={1}>
          <text fg={searching ? theme.accent : theme.fgDim} attributes={1}>
            /
          </text>
          <text fg={theme.fg}>{query}</text>
          {searching ? <text fg={theme.accent}>▌</text> : null}
          {!empty ? (
            <text fg={theme.fgDim}>
              {"  "}
              {matches} match{matches === 1 ? "" : "es"}
            </text>
          ) : null}
        </box>
      ) : null}
      {empty ? (
        <box flexGrow={1} alignItems="center" justifyContent="center">
          <text fg={theme.fgDim}>no matches for "{query.trim()}"</text>
        </box>
      ) : (
        <scrollbox
          focused={!searching}
          scrollY
          flexGrow={1}
          contentOptions={{ flexDirection: "row", gap: 2 }}
        >
          <Column blocks={left} />
          <Column blocks={right} />
        </scrollbox>
      )}
    </Modal>
  );
}
