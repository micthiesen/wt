import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";

import {
  pickAggregateState,
  registryStatusToState,
  type DerivedState,
} from "../../core/claude-status.ts";
import { getHarness } from "../../core/harness/index.ts";
import { claudeRegistryQuery } from "../../state/index.ts";
import { actionLineFg } from "../action-line-style.ts";
import { STATE_FG } from "../claude-state.ts";
import { useSessionRun } from "../hooks/useSessionRun.ts";
import {
  DOTFILES_SLOT,
  MAIN_CLONE_SLOT,
  WT_SOURCE_SLOT,
} from "../session-slots.ts";
import { theme } from "../theme.ts";

/** Claude Code robot glyph, reused from the harness registry so the
 *  slot status markers match the row/session badges. */
const CLAUDE_GLYPH = getHarness("claude").glyph;

export type FooterMode =
  | { kind: "legend" }
  | { kind: "toast"; message: string; color?: string }
  | {
      kind: "input";
      prompt: string;
      value: string;
      purpose: "new" | "rename-section";
      /**
       * Optional default `--base` ref for the new-worktree input (set
       * by the `N` keybinding). Not rendered in the prompt; the event
       * log carries the notice. An explicit `--base` in the input
       * text overrides this.
       */
      base?: string;
    };

type Props = {
  mode: FooterMode;
  hint?: string;
  height?: number;
};

/**
 * Live derived state for a session slot, matched by cwd against the
 * Claude registry (claude reports its project dir as cwd, so a slot's
 * sessions land under the slot path). Aggregates when more than one
 * session runs in the slot dir; null when none is live.
 */
function useSlotState(path: string): DerivedState | null {
  const registry = useQuery(claudeRegistryQuery());
  return useMemo(() => {
    const sessions = registry.data?.sessions ?? [];
    const states: DerivedState[] = [];
    for (const s of sessions) {
      if (s.cwd === path) states.push(registryStatusToState(s.status));
    }
    return pickAggregateState(states);
  }, [registry.data, path]);
}

/** Status color for a slot's robot glyph; dim when no live session. */
function slotGlyphFg(state: DerivedState | null): string {
  return state ? STATE_FG[state] : theme.fgDim;
}

export function Footer({ mode, hint }: Props) {
  // The two tail-less session slots (`,` wt-source and `/` dotfiles) get
  // permanent status robots bundled at the far right — wt-source first,
  // dotfiles to its right. No labels: position is the discriminator (the
  // main-clone slot is represented separately by its tail on the left).
  // Each robot's color tracks that slot's live state.
  const wtState = useSlotState(WT_SOURCE_SLOT.path);
  const dotfilesState = useSlotState(DOTFILES_SLOT.path);
  return (
    <box
      flexShrink={0}
      backgroundColor={theme.bgAlt}
      paddingLeft={1}
      paddingRight={1}
      height={1}
      flexDirection="row"
    >
      <box flexDirection="row" flexGrow={1} flexShrink={1} overflow="hidden">
        {mode.kind === "legend" ? <MainSlotTail /> : null}
        {mode.kind === "toast" ? (
          <text fg={mode.color ?? theme.ok}>{mode.message}</text>
        ) : null}
        {mode.kind === "input" ? (
          <>
            <text>
              <span fg={theme.accent} attributes={1}>
                {mode.prompt}
              </span>
              <span> </span>
              <span fg={theme.fgBright}>{mode.value}</span>
              <span fg={theme.accent}>█</span>
            </text>
            <text fg={theme.fgDim}> (⏎ submit, esc cancel)</text>
          </>
        ) : null}
      </box>
      {hint ? (
        <box flexShrink={0} flexDirection="row">
          <text fg={theme.fgDim}>{hint}</text>
        </box>
      ) : null}
      <box flexShrink={0} marginLeft={1} flexDirection="row">
        <box width={2} flexShrink={0}>
          <text fg={slotGlyphFg(wtState)}>{CLAUDE_GLYPH}</text>
        </box>
        <box width={2} flexShrink={0}>
          <text fg={slotGlyphFg(dotfilesState)}>{CLAUDE_GLYPH}</text>
        </box>
      </box>
    </box>
  );
}

/**
 * Single-line tail of the main-clone session — the bottom bar's
 * default-mode content. A status-colored robot glyph leads (it stands in
 * for the slot label, so the `main` text is gone), followed directly by
 * the latest `ActionLine` colored per its kind (so an assistant reply
 * reads as plain text, a tool-error as red, etc.). When no session is
 * live or no lines have arrived yet (pre-creation race), falls back to a
 * dim idle hint that still surfaces `.` as the start key and `?` for
 * help. The tail is claude-jsonl-only; if the user spawns a
 * codex/opencode session via `.` (the active primary harness) the bar
 * reads as "idle" because there's no jsonl to tail. That's a known v1
 * trade-off — bottom-bar feedback for non-claude harnesses would mean
 * wiring their event streams in here too.
 */
function MainSlotTail() {
  const run = useSessionRun(MAIN_CLONE_SLOT.slug, null);
  const glyphFg = slotGlyphFg(useSlotState(MAIN_CLONE_SLOT.path));
  const lastLine =
    run && run.lines.length > 0 ? run.lines[run.lines.length - 1] : null;
  if (!lastLine) {
    return (
      <text wrapMode="none" truncate>
        <span fg={glyphFg}>{CLAUDE_GLYPH}  </span>
        <span fg={theme.fgDim}>idle  ·  </span>
        <span fg={theme.accent}>.</span>
        <span fg={theme.fgDim}> start  ·  </span>
        <span fg={theme.accent}>?</span>
        <span fg={theme.fgDim}> help</span>
      </text>
    );
  }
  return (
    <text wrapMode="none" truncate>
      <span fg={glyphFg}>{CLAUDE_GLYPH}  </span>
      <span fg={actionLineFg(lastLine.kind)}>{lastLine.text}</span>
    </text>
  );
}
