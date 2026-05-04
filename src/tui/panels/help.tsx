import type { ReactNode } from "react";

import { NF } from "../icons.ts";
import { Modal } from "../modal.tsx";
import { Spinner } from "../spinner.tsx";
import { theme } from "../theme.ts";

type Section = { title: string; items: [string, string][] };
type LegendGlyph = ReactNode;

const SECTIONS: Section[] = [
  {
    title: "navigation",
    items: [
      ["j / ↓", "down"],
      ["k / ↑", "up"],
      ["g", "top"],
      ["G", "bottom"],
      ["/", "filter list"],
      ["esc", "clear filter"],
    ],
  },
  {
    title: "organize",
    items: [
      ["l", "set section (picker)"],
      ["l l", "new section (chord)"],
      ["L", "rename current section"],
      ["J / K", "move row · across sections"],
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
      ["y", "yank menu (b/s/S/p/n/i)"],
      ["t", "regenerate AI summary"],
      ["m", "merge when ready (toggle)"],
      ["e", "exit draft (mark ready for review)"],
      ["v", "edit reviewers (picker)"],
      ["!", "run claude action · kill if running"],
      ["F12", "enter claude session · F12 again to detach"],
      ["⇧F12", "kill claude session (preserves history)"],
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
      ["R", "clear all cached data"],
      [",", "open config in zed"],
      [".", "open wt source in zed"],
      ["?", "toggle this help"],
      ["q / ^C", "quit"],
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
  [`${NF.mergeQueue} N`, theme.ok, "MQ pos N · mergeable"],
  [`${NF.mergeQueue} N`, theme.warn, "MQ pos N · awaiting/queued"],
  [`${NF.mergeQueue} N`, theme.err, "MQ pos N · blocked"],
  [NF.mergeQueue, theme.info, "auto-merge armed (waiting)"],
  [NF.bolt, theme.warn, "SST stage deployed"],
  [NF.comment, theme.ok, "Claude · `!` action running"],
  [NF.comment, theme.accent, "Claude · interactive session live"],
  [NF.comment, theme.warn, "Claude · waiting (details pane)"],
  [NF.comment, theme.fgDim, "Claude · idle"],
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
        ["?", "toggle"],
        ["esc / q", "close"],
      ]}
    >
      <box flexDirection="row" flexGrow={1}>
        <box flexDirection="column" flexGrow={1} flexBasis={0}>
          {SECTIONS.map((sec) => (
            <box key={sec.title} flexDirection="column" marginBottom={1}>
              <text fg={theme.fgDim} attributes={1}>
                {sec.title}
              </text>
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
      </box>
    </Modal>
  );
}
