import { actionLineFg } from "../action-line-style.ts";
import { useSessionRun } from "../hooks/useSessionRun.ts";
import { MAIN_CLONE_SLOT } from "../session-slots.ts";
import { theme } from "../theme.ts";

export type FooterMode =
  | { kind: "legend" }
  | { kind: "confirm"; message: string; pendingKey: string }
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
    }
  | { kind: "filter"; value: string };

type Props = {
  mode: FooterMode;
  hint?: string;
  height?: number;
};

export function Footer({ mode, hint }: Props) {
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
        {mode.kind === "confirm" ? (
          <text fg={theme.warn}>{mode.message}</text>
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
        {mode.kind === "filter" ? (
          <>
            <text>
              <span fg={theme.accent} attributes={1}>
                /
              </span>
              <span fg={theme.fgBright}>{mode.value}</span>
              <span fg={theme.accent}>█</span>
            </text>
            <text fg={theme.fgDim}> (⏎ apply, esc clear)</text>
          </>
        ) : null}
      </box>
      {hint ? (
        <box flexShrink={0} flexDirection="row">
          <text fg={theme.fgDim}>{hint}</text>
        </box>
      ) : null}
    </box>
  );
}

/**
 * Single-line tail of the main-clone session — the bottom bar's
 * default-mode content. Renders the slot label, a dim separator, and
 * the latest `ActionLine` colored per its kind (so an assistant reply
 * reads as plain text, a tool-error as red, etc.). When no session is
 * live or no lines have arrived yet (pre-creation race), falls back
 * to a dim idle hint that still surfaces `,` as the start key and `?`
 * for help. The tail is claude-jsonl-only; if the user spawns a
 * codex/opencode session via `,` (because that's the active primary
 * harness) the bar reads as "idle" because there's no jsonl to tail.
 * That's a known v1 trade-off — bottom-bar feedback for non-claude
 * harnesses would mean wiring their event streams in here too.
 */
function MainSlotTail() {
  const run = useSessionRun(MAIN_CLONE_SLOT.slug, null);
  const lastLine =
    run && run.lines.length > 0 ? run.lines[run.lines.length - 1] : null;
  if (!lastLine) {
    return (
      <text wrapMode="none" truncate>
        <span fg={theme.accent}>{MAIN_CLONE_SLOT.label}</span>
        <span fg={theme.fgDim}> · idle  ·  </span>
        <span fg={theme.accent}>,</span>
        <span fg={theme.fgDim}> start  ·  </span>
        <span fg={theme.accent}>?</span>
        <span fg={theme.fgDim}> help</span>
      </text>
    );
  }
  return (
    <text wrapMode="none" truncate>
      <span fg={theme.accent}>{MAIN_CLONE_SLOT.label}</span>
      <span fg={theme.fgDim}> · </span>
      <span fg={actionLineFg(lastLine.kind)}>{lastLine.text}</span>
    </text>
  );
}
