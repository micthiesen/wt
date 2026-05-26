import type { ReactNode } from "react";

import { getHarness } from "../../core/harness/index.ts";
import { STATE_DOT, STATE_FG } from "../claude-state.ts";
import { NF } from "../icons.ts";
import { Modal } from "../modal.tsx";
import { Spinner } from "../spinner.tsx";
import { theme } from "../theme.ts";

type Section = {
  title: string;
  items: [string, string][];
  /** Optional one-line prose above the keys — for sections whose
   *  mental model isn't obvious from the keys alone. */
  note?: string;
};
type LegendGlyph = ReactNode;

const SECTIONS: Section[] = [
  {
    title: "navigation",
    items: [
      ["j / ↓", "down"],
      ["k / ↑", "up"],
      ["g", "top"],
      ["G", "bottom"],
    ],
  },
  {
    title: "organize",
    items: [
      ["l", "set section (picker) · + stack section for current chain"],
      ["l l", "confirm highlighted section"],
      ["l n", "new section (chord)"],
      ["L", "rename current section"],
      ["J / K", "move row · across sections"],
      ["{ / }", "move section up / down"],
      ["a", "archive / restore"],
    ],
  },
  {
    title: "worktree",
    items: [
      ["o", "open in zed"],
      ["p", "open PR in browser"],
      ["i", "open linear issue"],
      ["s", "open deployed app"],
      ["y", "yank menu (b/s/S/p/n/i/r)"],
      ["t", "regenerate AI summary"],
      ["m", "merge when ready (toggle)"],
      ["b", "set stack base (parent branch) for this worktree"],
      ["e", "exit draft (mark ready for review)"],
      ["E", "ship: mark ready + request default_reviewer + arm auto-merge"],
      ["v", "edit reviewers (picker · v v submits)"],
      ["!", "run claude action · kill if running"],
      ["! c", "open action picker, jump to custom prompt"],
      ["F10", "enter shell · F10 again to detach"],
      ["⇧F10", "kill shell session (ends background procs)"],
      ["F11", "enter diff TUI · F11 again to detach"],
      ["⇧F11", "kill diff session (resets gitu state)"],
      ["F12", "enter F12-target session · F12 again to detach"],
      ["⇧F12", "harness picker (one-off spawn)"],
      ["TAB", "cycle primary harness (top-right)"],
      [";", "sessions picker (all harnesses for current row)"],
      ["; c / x / o", "jump to + new claude / codex / opencode"],
      ["; x", "kill highlighted session"],
      ["d", "remove worktree"],
    ],
  },
  {
    title: "global",
    items: [
      ["n", "new worktree"],
      ["N", "new worktree · base = selected"],
      ["c", "clean merged/gone"],
      ["r", "refresh (fetch + recompute)"],
      ["^R", "clear all cached data"],
      [",", "enter wt source session · F12 to detach"],
      [".", "enter main repo session · F12 to detach"],
      ["/", "enter dotfiles session · F12 to detach"],
      [">", "open wt source in zed"],
      ["?", "toggle this help"],
      ["q / ^C", "quit"],
    ],
  },
  {
    title: "review requests (pinned section)",
    note: "PRs awaiting your review, pulled from GitHub. Not worktrees — only these keys apply.",
    items: [["p / ⏎", "open PR on GitHub"]],
  },
  {
    title: "outputs (bottom pane · per-worktree)",
    note: "each worktree remembers its last-shown output; switching rows restores it.",
    items: [
      ["'", "outputs picker for this worktree"],
      ["' '", "confirm highlighted output (chord)"],
      ["[ / ]", "cycle prev / next output for this worktree"],
      ["\"", "jump to events for this worktree"],
      ["esc", "clear focus (return to follow-row auto)"],
    ],
  },
  {
    title: "modals",
    note: "every list picker follows the same trigger-key-confirm pattern. esc cancels; j/k or arrows move; the bottom pane previews the highlight when applicable. CLAUDE.md has the full rules.",
    items: [
      ["<trigger>", "open picker"],
      ["<trigger> <trigger>", "confirm highlighted"],
      ["⏎", "confirm highlighted"],
      ["esc / q", "cancel"],
      ["1-9", "quick pick by row digit"],
    ],
  },
];

const STATUS_GLYPHS: [LegendGlyph, string, string][] = [
  [NF.rocket, theme.accent, "busy (init/install)"],
  [NF.trash, theme.err, "busy (removing)"],
  [NF.unlink, theme.err, "missing (path vanished)"],
  [NF.slash, theme.warn, "gone (branch deleted upstream)"],
  [NF.merge, theme.ok, "merged into origin/main"],
  [NF.pencil, theme.warn, "uncommitted changes"],
  [<Spinner key="refresh" fg={theme.fgDim} />, theme.fgDim, "refreshing"],
  ["  ", theme.fgDim, "idle / clean"],
];

const BADGES: [LegendGlyph, string, string][] = [
  [NF.prOpen, theme.accentAlt, "PR open"],
  [NF.prDraft, theme.fgDim, "PR draft"],
  [NF.prMerged, theme.info, "PR merged"],
  [NF.prClosed, theme.err, "PR closed"],
  [NF.checkPass, theme.ok, "CI checks passing"],
  [NF.checkFail, theme.err, "CI checks failing"],
  [NF.checkPend, theme.warn, "CI checks pending"],
  [`${NF.mergeQueue} N`, theme.ok, "merge queue pos N · mergeable"],
  [`${NF.mergeQueue} N`, theme.warn, "merge queue pos N · awaiting/queued"],
  [`${NF.mergeQueue} N`, theme.err, "merge queue pos N · blocked"],
  [NF.mergeQueue, theme.info, "auto-merge armed (waiting)"],
  [NF.bolt, theme.warn, "SST stage deployed"],
  [NF.comment, theme.ok, "Claude · `!` action running"],
  [
    getHarness("claude").glyph,
    getHarness("claude").color,
    "AI session live (list pane shows the F12-target harness glyph)",
  ],
  [STATE_DOT.working, STATE_FG.working, "AI session · working"],
  [STATE_DOT.asking, STATE_FG.asking, "AI session · asking"],
  [STATE_DOT.polling, STATE_FG.polling, "AI session · polling"],
  [STATE_DOT.unknown, STATE_FG.unknown, "AI session · unknown"],
  [STATE_DOT.waiting, STATE_FG.waiting, "AI session · waiting"],
  [STATE_DOT.idle, STATE_FG.idle, "AI session · idle"],
  [STATE_DOT.abandoned, STATE_FG.abandoned, "AI session · abandoned"],
];

function KeyRow({ keyText, label }: { keyText: string; label: string }) {
  return (
    <box flexDirection="row">
      <box width={10} flexShrink={0}>
        <text fg={theme.accent} attributes={1}>
          {keyText}
        </text>
      </box>
      <text fg={theme.fg}>{label}</text>
    </box>
  );
}

function GlyphRow({
  glyph,
  color,
  label,
}: {
  glyph: LegendGlyph;
  color: string;
  label: string;
}) {
  return (
    <box flexDirection="row">
      <box width={5} flexShrink={0}>
        {typeof glyph === "string" ? <text fg={color}>{glyph}</text> : glyph}
      </box>
      <text fg={theme.fg}>{label}</text>
    </box>
  );
}

export function HelpOverlay() {
  return (
    <Modal
      title="help"
      inset={{ top: "8%", right: "10%", bottom: "8%", left: "10%" }}
      hints={[
        ["j k / ↑ ↓", "scroll"],
        ["?", "toggle"],
        ["esc / q", "close"],
      ]}
    >
      <scrollbox
        focused
        scrollY
        flexGrow={1}
        contentOptions={{ flexDirection: "row" }}
      >
        <box flexDirection="column" flexGrow={1} flexBasis={0}>
          {SECTIONS.map((sec) => (
            <box key={sec.title} flexDirection="column" marginBottom={1}>
              <text fg={theme.fgDim} attributes={1}>
                {sec.title}
              </text>
              {sec.note ? (
                <text fg={theme.fgDim} wrapMode="word">
                  {sec.note}
                </text>
              ) : null}
              {sec.items.map(([k, l]) => (
                <KeyRow key={k} keyText={k} label={l} />
              ))}
            </box>
          ))}
        </box>
        <box flexDirection="column" flexGrow={1} flexBasis={0}>
          <box flexDirection="column" marginBottom={1}>
            <text fg={theme.fgDim} attributes={1}>
              status glyphs
            </text>
            {STATUS_GLYPHS.map(([g, c, l]) => (
              <GlyphRow key={l} glyph={g} color={c} label={l} />
            ))}
          </box>
          <box flexDirection="column" marginBottom={1}>
            <text fg={theme.fgDim} attributes={1}>
              row badges
            </text>
            {BADGES.map(([g, c, l]) => (
              <GlyphRow key={l} glyph={g} color={c} label={l} />
            ))}
          </box>
          <box flexDirection="column" marginBottom={1}>
            <text fg={theme.fgDim} attributes={1}>
              sync notation
            </text>
            <text fg={theme.fg}>
              <span fg={theme.fgDim}>(↑N ↓M)</span> vs remote branch
            </text>
            <text fg={theme.fg}>
              <span fg={theme.fgDim}>[↑N ↓M]</span> vs origin/main
            </text>
            <text fg={theme.fg}>
              <span fg={theme.warn}>↑</span> ahead ·{" "}
              <span fg={theme.err}>↓</span> behind
            </text>
          </box>
          <box flexDirection="column" marginBottom={1}>
            <text fg={theme.fgDim} attributes={1}>
              new: prompt flags
            </text>
            <text fg={theme.fg}>
              <span fg={theme.accent}>--any</span>
              <span fg={theme.fgDim}> match any author</span>
            </text>
            <text fg={theme.fg}>
              <span fg={theme.accent}>--base </span>
              <span fg={theme.fgDim}>{"<ref>"} branch off ref</span>
            </text>
          </box>
          <box flexDirection="column">
            <text fg={theme.fgDim} attributes={1}>
              selection
            </text>
            <text fg={theme.fg}>drag mouse → auto-copies</text>
          </box>
        </box>
      </scrollbox>
    </Modal>
  );
}
