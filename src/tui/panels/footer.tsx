import type { DerivedState } from "../../core/claude-status.ts";
import { getHarness, type HarnessId } from "../../core/harness/index.ts";
import { actionLineFg } from "../action-line-style.ts";
import { stateColor } from "../claude-state.ts";
import { useActiveSessionsBySlug } from "../hooks/useHarnessSessions.ts";
import { usePrimaryHarness } from "../hooks/usePrimaryHarness.ts";
import { useSessionRun } from "../hooks/useSessionRun.ts";
import {
  DOTFILES_SLOT,
  MAIN_CLONE_SLOT,
  SESSION_SLOTS,
  WT_SOURCE_SLOT,
} from "../session-slots.ts";
import { theme } from "../theme.ts";

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

/** Status color for a slot's robot glyph; dim when no live session. */
function slotGlyphFg(harnessId: HarnessId, state: DerivedState | null): string {
  return state ? stateColor(harnessId, state) : theme.fgDim;
}

export function Footer({ mode, hint }: Props) {
  // The two tail-less session slots (`,` wt-source and `/` dotfiles) get
  // permanent status robots bundled at the far right — wt-source first,
  // dotfiles to its right. No labels: position is the discriminator (the
  // main-clone slot is represented separately by its tail on the left).
  // Each robot's glyph AND color follow the TAB-selected primary harness:
  // a slot keybind always opens the primary harness, so we track that
  // harness's live session in the slot (not the cross-harness F12 target
  // the list rows use). TABbing therefore moves both the icon and the
  // status color together — and a dim glyph means "no live primary-harness
  // session in this slot", not "no session at all".
  const primary = usePrimaryHarness();
  const primaryGlyph = getHarness(primary).glyph;
  const slotSessions = useActiveSessionsBySlug(SESSION_SLOTS, primary, primary);
  const wtState = slotSessions.get(WT_SOURCE_SLOT.slug)?.state ?? null;
  const dotfilesState = slotSessions.get(DOTFILES_SLOT.slug)?.state ?? null;
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
        {mode.kind === "legend" ? (
          <MainSlotTail
            state={slotSessions.get(MAIN_CLONE_SLOT.slug)?.state ?? null}
          />
        ) : null}
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
          <text fg={slotGlyphFg(primary, wtState)}>{primaryGlyph}</text>
        </box>
        <box width={2} flexShrink={0}>
          <text fg={slotGlyphFg(primary, dotfilesState)}>{primaryGlyph}</text>
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
 * help. The leading glyph's color reflects the slot's live state across
 * any harness (passed in via `state`), but the trailing tail TEXT is
 * claude-jsonl-only: a codex/opencode session in the slot still lights
 * the glyph yet shows no line text, since there's no jsonl to tail.
 * Wiring those event streams into the tail text is a known v1 trade-off.
 */
function MainSlotTail({ state }: { state: DerivedState | null }) {
  const run = useSessionRun(MAIN_CLONE_SLOT.slug, null);
  const primary = usePrimaryHarness();
  const glyphFg = slotGlyphFg(primary, state);
  const primaryGlyph = getHarness(primary).glyph;
  const lastLine =
    run && run.lines.length > 0 ? run.lines[run.lines.length - 1] : null;
  if (!lastLine) {
    return (
      <text wrapMode="none" truncate>
        <span fg={glyphFg}>{primaryGlyph}  </span>
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
      <span fg={glyphFg}>{primaryGlyph}  </span>
      <span fg={actionLineFg(lastLine.kind)}>{lastLine.text}</span>
    </text>
  );
}
