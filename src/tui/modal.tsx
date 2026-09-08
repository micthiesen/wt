import type { ReactNode } from "react";
import { useTerminalDimensions } from "@opentui/react";

import { KeyHint, type KeyHintPair } from "./key-hint.tsx";
import { theme } from "./theme.ts";

type Percent = `${number}%`;

type Inset = {
  top?: Percent;
  right?: Percent;
  bottom?: Percent;
  left?: Percent;
};

const DEFAULT_INSET: Required<Inset> = {
  top: "20%",
  right: "20%",
  bottom: "20%",
  left: "20%",
};

/**
 * Below this terminal width the viewport-relative insets stop making
 * sense — 20% of a narrow terminal leaves an unusably thin modal.
 * Narrow viewports get a full-width, near-full-height frame instead
 * (caller insets included: whatever margin looked right at 130 cols
 * is wrong at 35).
 */
const NARROW_WIDTH = 60;

const NARROW_INSET: Required<Inset> = {
  top: "5%",
  right: "0%",
  bottom: "5%",
  left: "0%",
};

/**
 * Rendered modal width (border included) never grows past this many
 * cells. Percent insets alone keep stretching with the terminal — at
 * 200 cols the default 20% side insets leave ~40-col gutters of bare
 * background either side, which reads as disconnected fragments
 * rather than a centered dialog. Only kicks in once the percent
 * insets would exceed it (see `capWidth`); narrow terminals can never
 * reach it, so `NARROW_INSET` above is untouched by this.
 */
const MAX_CONTENT_WIDTH = 100;

type Props = {
  /** Title format: `name [· subtitle]`. Never include keystroke hints. */
  title: string;
  /**
   * Border + title color. Defaults to `theme.accent` (non-destructive).
   * Use `theme.warn` for confirm-before-irreversible-action modals.
   */
  borderColor?: string;
  /** Viewport-relative padding. Smaller values yield a larger modal. */
  inset?: Inset;
  /**
   * Maximum visible frame width in cells. `null` keeps the inset-derived
   * width, for wide content surfaces such as terminal logs.
   */
  maxWidth?: number | null;
  /**
   * Keystroke hints rendered along the bottom edge. Pass an empty
   * array only if the modal has no dismiss path (it always has at
   * least one — esc/q/ctrl+c are universal).
   */
  hints: KeyHintPair[];
  /**
   * `true` pins the frame to the full inset-derived rectangle (the
   * pre-auto-height behavior) — for content that should OWN the space,
   * like the help overlay's scrolling sections. Default (false) sizes
   * the modal to its content, capped at that same rectangle.
   */
  fill?: boolean;
  children: ReactNode;
};

/**
 * Modal conventions every caller should follow:
 *
 *   1. **Trigger re-press.** For a plain overlay (no selection to make),
 *      the key that opened it also closes it (`?` opens & closes help).
 *      For a list/multi-select picker, re-pressing the trigger instead
 *      CONFIRMS the highlight — it's the same shape as `Enter`, not a
 *      dismiss (`v` opens the reviewer picker, `v v` submits the
 *      selected set; same for `l l`, `; ;`, `b b`, `u u`). Either way,
 *      always accept the universal `esc` / `q` / `ctrl+c` dismiss keys
 *      too, so muscle-memory works in both directions.
 *   2. **Universal dismiss.** Always accept `esc`, `q`, and `ctrl+c`.
 *   3. **Hints.** List dismiss keys in the `hints` prop so the user
 *      sees them along the bottom edge.
 */
export function Modal({
  title,
  borderColor = theme.accent,
  inset,
  maxWidth = MAX_CONTENT_WIDTH,
  hints,
  fill = false,
  children,
}: Props) {
  const { width, height } = useTerminalDimensions();
  if (process.env.WT_POPUP === "1") {
    // tmux already bounds the popup; a second width cap leaves bare terminal
    // columns beside the painted modal.
    fill = true;
    maxWidth = null;
  }
  const i =
    process.env.WT_POPUP === "1"
      ? { top: "0%", right: "0%", bottom: "0%", left: "0%" } as const
      : width < NARROW_WIDTH ? NARROW_INSET : { ...DEFAULT_INSET, ...inset };
  const { left, right } =
    maxWidth === null
      ? { left: i.left, right: i.right }
      : capWidth(i.left, i.right, width, maxWidth);
  // Height is content-driven by default: the box grows with its
  // children and the vertical insets only bound the MAXIMUM. A seven-
  // row picker renders as a seven-row modal instead of a fixed
  // 60%-tall frame of mostly empty space. `fill` keeps the full frame
  // for content that owns the space (help's scrolling sections — a
  // bare flexGrow scrollbox doesn't self-measure, so auto-height
  // would collapse it).
  const maxHeight = Math.max(
    8,
    Math.floor((height * (100 - pct(i.top) - pct(i.bottom))) / 100),
  );
  return (
    // The outer box paints a 1-cell gutter either side of the border:
    // the pane behind keeps rendering, and without the gutter its text
    // sits flush against `║` — a sentence bisected by the modal edge
    // reads as garbled overprint rather than background.
    <box
      position="absolute"
      top={i.top}
      left={left}
      right={right}
      {...(fill ? { bottom: i.bottom } : { maxHeight })}
      zIndex={10}
      backgroundColor={theme.bg}
      paddingLeft={1}
      paddingRight={1}
      flexDirection="column"
    >
      <box
        backgroundColor={theme.bg}
        border
        borderStyle="double"
        borderColor={borderColor}
        title={` ${title} `}
        titleAlignment="left"
        padding={1}
        flexDirection="column"
        flexShrink={1}
        minHeight={0}
        {...(fill ? { flexGrow: 1 } : {})}
      >
        <box
          flexDirection="column"
          flexShrink={1}
          minHeight={0}
          overflow="hidden"
          {...(fill ? { flexGrow: 1 } : {})}
        >
          {children}
        </box>
        {/* flexWrap: the hint chips flow onto extra rows at narrow widths
            instead of overrunning the border. Each chip is one unbreakable
            <text>; wrapping happens only between chips. */}
        <box flexShrink={0} flexDirection="row" flexWrap="wrap" marginTop={1}>
          <KeyHint pairs={hints} />
        </box>
      </box>
    </box>
  );
}

function pct(p: Percent): number {
  const n = parseFloat(p);
  return Number.isFinite(n) ? n : 0;
}

/**
 * Resolves the outer box's left/right insets, capping the rendered
 * modal (border included) at the caller's maximum. The percent insets
 * pass through unchanged until the terminal is wide enough that they'd
 * produce a wider modal than the cap — at that point we switch to
 * absolute-cell insets that center a fixed-width modal instead of
 * continuing to stretch it. `+2` accounts for the outer box's own
 * 1-cell gutter padding on each side (see the JSX below) between the
 * inset and the visible double border.
 */
function capWidth(
  left: Percent,
  right: Percent,
  termWidth: number,
  maxWidth: number,
): { left: Percent | number; right: Percent | number } {
  const outerWidth = termWidth - (termWidth * pct(left)) / 100 - (termWidth * pct(right)) / 100;
  const contentWidth = outerWidth - 2;
  if (contentWidth <= maxWidth) return { left, right };
  const gutter = Math.floor((termWidth - (maxWidth + 2)) / 2);
  return { left: gutter, right: gutter };
}
