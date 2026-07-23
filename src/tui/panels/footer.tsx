import type { DerivedState } from "../../core/harness/status.ts";
import { getHarness, type HarnessId } from "../../core/harness/index.ts";
import { actionLineFg } from "../action-line-style.ts";
import { stateColor } from "../claude-state.ts";
import { useActiveSessionsBySlug } from "../hooks/useHarnessSessions.ts";
import { usePrimaryHarness } from "../hooks/usePrimaryHarness.ts";
import { useHarnessRun, useSessionRun } from "../hooks/useSessionRun.ts";
import {
  DOTFILES_SLOT,
  MAIN_CLONE_SLOT,
  SESSION_SLOTS,
  WT_SOURCE_SLOT,
} from "../sessions/slots.ts";
import { theme } from "../theme.ts";

export type FooterMode =
  | { kind: "legend" }
  | { kind: "toast"; message: string; color?: string }
  | {
      kind: "input";
      prompt: string;
      value: string;
      purpose: "new" | "new-remote" | "rename-section";
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
  /**
   * Hub mode: legend renders as bare harness status glyphs (main slot
   * included) with no last-message preview — the ~35-col task pane has
   * no room for tail text. Toast / input modes are unaffected.
   */
  compact?: boolean;
};

/** Status color for a slot's robot glyph; dim when no live session. */
function slotGlyphFg(harnessId: HarnessId, state: DerivedState | null): string {
  return state ? stateColor(harnessId, state) : theme.fgDim;
}

export function Footer({ mode, hint, compact = false }: Props) {
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
          compact ? (
            <MainSlotGlyph
              state={slotSessions.get(MAIN_CLONE_SLOT.slug)?.state ?? null}
            />
          ) : (
            <MainSlotTail
              state={slotSessions.get(MAIN_CLONE_SLOT.slug)?.state ?? null}
            />
          )
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
 * help. Both the glyph and the trailing tail TEXT follow the TAB-
 * selected primary harness: claude reads its jsonl tail, codex/opencode
 * read their `harnessTailRegistry` trail (rollout jsonl / SQLite). The
 * three tail hooks are all called unconditionally (rules of hooks); we
 * pick the primary's run. A non-primary harness session in the slot
 * lights nothing here — the slot keybind opens the primary, so the bar
 * tracks the primary, same as the slot glyphs above.
 */
/** Compact-legend variant: the main slot's status robot, nothing else. */
function MainSlotGlyph({ state }: { state: DerivedState | null }) {
  const primary = usePrimaryHarness();
  return (
    <text fg={slotGlyphFg(primary, state)}>{getHarness(primary).glyph}</text>
  );
}

function MainSlotTail({ state }: { state: DerivedState | null }) {
  const primary = usePrimaryHarness();
  const claudeRun = useSessionRun(MAIN_CLONE_SLOT.slug, null);
  const codexRun = useHarnessRun(MAIN_CLONE_SLOT.slug, "codex");
  const opencodeRun = useHarnessRun(MAIN_CLONE_SLOT.slug, "opencode");
  const run =
    primary === "claude"
      ? claudeRun
      : primary === "codex"
        ? codexRun
        : opencodeRun;
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
