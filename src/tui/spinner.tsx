import { useEffect, useRef, useState } from "react";
import { useTimeline } from "@opentui/react";

/**
 * Two-arc rotation, 2-cell wide so it slots in alongside the
 * NF icons in the row trailer / status marker / legend without
 * throwing off alignment. Single source of truth — change the
 * frames or cadence here and every site that imports `<Spinner>`
 * or `useSpinnerFrame()` updates.
 */
const DOTS_FRAMES = ["◜◞", "◝◟", "◞◜", "◟◝"] as const;
const DOTS_FRAME_MS = 120;

/**
 * Classic ASCII bouncing ball — used for "thinking" indicators that
 * have more horizontal space (e.g. the AI summary line, where wrap
 * is fine and a wider, recognizable animation reads better than a
 * tight glyph).
 */
const BALL_FRAMES = [
  "( ●    )",
  "(  ●   )",
  "(   ●  )",
  "(    ● )",
  "(     ●)",
  "(    ● )",
  "(   ●  )",
  "(  ●   )",
  "( ●    )",
  "(●     )",
] as const;
const BALL_FRAME_MS = 80;

/**
 * Connected "traveling wave" for the header refresh indicator. A
 * triangle of block heights that loops seamlessly (rises ▁→█ then falls
 * back toward ▁). Rendered across N cells phase-shifted by position, so
 * one undulation flows through the whole strip rather than N independent
 * spinners. Width N encodes the in-flight query count.
 */
const WAVE_FRAMES = "▁▂▃▄▅▆▇█▇▆▅▄▃▂";
const WAVE_FRAME_MS = 90;
/** Cap so a big refresh fan-out can't overrun the header line. */
const MAX_WAVE_WIDTH = 12;
// Per-frame smoothing fractions for the wave width (see `useEased`). The
// raw in-flight count bounces around as queries fire/resolve in bursts;
// low-passing it makes the ribbon glide instead of flashing. Grows a
// little faster than it shrinks so a burst shows up promptly but drains
// gently.
const WAVE_GROW_ALPHA = 0.18;
const WAVE_SHRINK_ALPHA = 0.1;
// Height envelope. The wave normally undulates between block levels 1 (▁)
// and 8 (█); scaling every cell by an eased 0..1 amplitude lets the whole
// strip rise out of the floor when it first appears and sink back when it
// hides, instead of popping in at full height. Smaller `snap` than the
// width easer because amplitude is fractional, not integer-valued.
const WAVE_AMP_ALPHA = 0.2;
const WAVE_AMP_SNAP = 0.02;
/** U+2580 is the cell just below ▁ (U+2581); +level gives ▁..█ for 1..8. */
const BLOCK_BASE = 0x2580;

/** Cell width of {@link Spinner} — useful when sizing layout slots. */
export const SPINNER_WIDTH = 2;

/**
 * Shared frame ticker: advances a frame index `0..period-1` on a loop at
 * `frameMs` per frame. Single source of motion for both the rotating
 * spinner glyphs and the multi-cell {@link RefreshWave}.
 */
function useFrameIndex(period: number, frameMs: number): number {
  const cycleMs = frameMs * period;
  const [idx, setIdx] = useState(0);
  const timeline = useTimeline({ duration: cycleMs, loop: true });
  // Register the animation once on mount. `useTimeline` returns a fresh
  // (unregistered) Timeline on every render — only the one captured here
  // at mount is wired into the engine, so a stable empty dep list is
  // intentional.
  useEffect(() => {
    const target = { t: 0 };
    timeline.add(target, {
      t: period,
      duration: cycleMs,
      ease: "linear",
      onUpdate: () => {
        const next = Math.floor(target.t) % period;
        setIdx((prev) => (prev === next ? prev : next));
      },
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  return idx;
}

function useFrameLoop(frames: readonly string[], frameMs: number): string {
  return frames[useFrameIndex(frames.length, frameMs)]!;
}

/**
 * Exponential smoothing (a one-pole low-pass filter) toward `target`.
 * Each frame moves the shown value a fraction of the remaining distance,
 * so it glides instead of snapping — separate up/down fractions let it
 * grow and shrink at different rates. Snaps once within `snap` of the
 * target so it settles exactly rather than asymptoting forever (and so a
 * settled value stops re-rendering); the default suits integer-valued
 * targets, while a fractional envelope (e.g. a 0..1 amplitude) wants a
 * smaller threshold. `target` is read through a ref so the per-frame
 * callback always sees the latest without re-registering.
 */
function useEased(
  target: number,
  upAlpha: number,
  downAlpha: number,
  snap = 0.5,
): number {
  const [shown, setShown] = useState(target);
  const targetRef = useRef(target);
  targetRef.current = target;
  const timeline = useTimeline({ duration: 1000, loop: true });
  useEffect(() => {
    const driver = { t: 0 };
    timeline.add(driver, {
      t: 1,
      duration: 1000,
      ease: "linear",
      onUpdate: () => {
        setShown((cur) => {
          const tgt = targetRef.current;
          const diff = tgt - cur;
          if (Math.abs(diff) < snap) return cur === tgt ? cur : tgt;
          return cur + diff * (diff > 0 ? upAlpha : downAlpha);
        });
      },
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  return shown;
}

export const useSpinnerFrame = (): string =>
  useFrameLoop(DOTS_FRAMES, DOTS_FRAME_MS);

export const useBouncingBall = (): string =>
  useFrameLoop(BALL_FRAMES, BALL_FRAME_MS);

/**
 * Drop-in 2-cell rotating spinner. Use this anywhere a static refresh
 * glyph used to live — the row trailer, the list status marker, the
 * help-overlay legend, etc. Render as a direct child of a `<box>`,
 * not inside another `<text>`.
 */
export function Spinner({ fg }: { fg: string }) {
  const frame = useSpinnerFrame();
  return <text fg={fg}>{frame}</text>;
}

/**
 * Header refresh indicator: a strip of `count` cells (capped at
 * {@link MAX_WAVE_WIDTH}) showing one shared traveling wave. Cell `i`
 * renders the wave frame at `tick + i`, so the cells read as a single
 * connected undulation flowing across the strip. Renders nothing when
 * `count` is 0; the wave keeps flowing as the count drains, so a refresh
 * reads as a live, shrinking ribbon rather than a flickering number. A
 * 0..1 height envelope ramps the cells up from the floor on appearance
 * (and decays while hidden) so each reappearance grows in smoothly rather
 * than popping to full height.
 */
export function RefreshWave({ count, fg }: { count: number; fg: string }) {
  const tick = useFrameIndex(WAVE_FRAMES.length, WAVE_FRAME_MS);
  const target = Math.min(Math.max(count, 0), MAX_WAVE_WIDTH);
  // Width follows the *smoothed* count, so cells slide in/out one at a
  // time instead of the ribbon flashing as the raw count bounces.
  const width = Math.round(useEased(target, WAVE_GROW_ALPHA, WAVE_SHRINK_ALPHA));
  // Height envelope: targets 1 while visible, 0 while hidden. It keeps
  // easing even while `width` is 0 (this runs before the early return), so
  // by the time the strip reappears the amplitude has drained to ~0 and
  // the wave rises back out of the floor.
  const amp = useEased(width > 0 ? 1 : 0, WAVE_AMP_ALPHA, WAVE_AMP_ALPHA, WAVE_AMP_SNAP);
  if (width <= 0) return null;
  const len = WAVE_FRAMES.length;
  let s = "";
  for (let i = 0; i < width; i++) {
    const full = WAVE_FRAMES.charCodeAt((tick + i) % len) - BLOCK_BASE; // 1..8
    const level = Math.round(full * amp); // 0..8; 0 reads as flat floor
    s += level <= 0 ? " " : String.fromCharCode(BLOCK_BASE + level);
  }
  return <text fg={fg}>{s}</text>;
}
