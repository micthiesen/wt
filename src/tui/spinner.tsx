import { useEffect, useRef, useState } from "react";

/**
 * All TUI animation rides ONE shared 100ms ticker, and the ticker only
 * exists while at least one animated component is mounted and active.
 *
 * This replaces the OpenTUI Timeline engine on purpose: a playing
 * Timeline holds a renderer-wide "live" request that pins the render
 * loop into continuous ~60fps mode, where EVERY frame re-walks the
 * whole renderable tree and repaints it — and `requestRender()`
 * becomes a no-op, so a keypress can't pull a frame forward. Two
 * forever-looping timelines here (the wave + its easer) kept the app
 * in that mode from boot to quit: ~13% CPU at complete rest, and a
 * standing tax on input latency that grew with board size. A plain
 * refcounted interval gets frame-accurate-enough animation at 10fps
 * (one on-demand frame per tick) and drops to ZERO frames when
 * nothing animates. Do not reintroduce `useTimeline` (or
 * `requestAnimationFrame`) for chrome animation — any of them re-arms
 * live mode.
 */
const TICK_MS = 100;

type Listener = () => void;

class AnimationTicker {
  private listeners = new Set<Listener>();
  private timer: Timer | null = null;
  private tick = 0;

  getTick = (): number => this.tick;

  subscribe = (fn: Listener): (() => void) => {
    this.listeners.add(fn);
    if (this.timer === null) {
      this.timer = setInterval(() => {
        this.tick++;
        // All listeners fire in one task, so React batches the
        // resulting setStates into a single commit → one frame per
        // tick however many spinners are on screen.
        for (const l of this.listeners) l();
      }, TICK_MS);
    }
    return () => {
      this.listeners.delete(fn);
      if (this.listeners.size === 0 && this.timer !== null) {
        clearInterval(this.timer);
        this.timer = null;
      }
    };
  };
}

const ticker = new AnimationTicker();

/**
 * Monotonic tick that advances every {@link TICK_MS} while `active`,
 * and freezes (releasing the shared interval) when not. Mount-scoped
 * animated components can pass `true`; always-mounted ones must gate
 * on visibility or the interval never sleeps.
 */
export function useAnimationTick(active: boolean): number {
  const [tick, setTick] = useState(ticker.getTick);
  useEffect(() => {
    if (!active) return;
    setTick(ticker.getTick());
    return ticker.subscribe(() => setTick(ticker.getTick()));
  }, [active]);
  return tick;
}

/**
 * Two-arc rotation, 2-cell wide so it slots in alongside the
 * NF icons in the row trailer / status marker / legend without
 * throwing off alignment. Single source of truth — change the
 * frames here and every site that imports `<Spinner>` or
 * `useSpinnerFrame()` updates.
 */
const DOTS_FRAMES = ["◜◞", "◝◟", "◞◜", "◟◝"] as const;

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

/**
 * Connected "traveling wave" for the header refresh indicator. A
 * triangle of block heights that loops seamlessly (rises ▁→█ then falls
 * back toward ▁). Rendered across N cells phase-shifted by position, so
 * one undulation flows through the whole strip rather than N independent
 * spinners. Width N encodes the in-flight query count.
 */
const WAVE_FRAMES = "▁▂▃▄▅▆▇█▇▆▅▄▃▂";
/** Cap so a big refresh fan-out can't overrun the header line. */
export const MAX_WAVE_WIDTH = 12;
// Per-tick smoothing fractions for the wave width (see `ease`). The
// raw in-flight count bounces around as queries fire/resolve in bursts;
// low-passing it makes the ribbon glide instead of flashing. Grows a
// little faster than it shrinks so a burst shows up promptly but drains
// gently. Tuned for the 10Hz tick (the old per-frame values were ~6x
// smaller because they applied ~60x/second).
const WAVE_GROW_ALPHA = 0.7;
const WAVE_SHRINK_ALPHA = 0.45;
// Height envelope. The wave normally undulates between block levels 1 (▁)
// and 8 (█); scaling every cell by an eased 0..1 amplitude lets the whole
// strip rise out of the floor when it first appears and sink back when it
// hides, instead of popping in at full height. Smaller `snap` than the
// width easer because amplitude is fractional, not integer-valued.
const WAVE_AMP_ALPHA = 0.7;
const WAVE_AMP_SNAP = 0.02;
/** U+2580 is the cell just below ▁ (U+2581); +level gives ▁..█ for 1..8. */
const BLOCK_BASE = 0x2580;

/**
 * Exponential smoothing (a one-pole low-pass filter) toward `tgt`:
 * move a fraction of the remaining distance per tick, snapping once
 * within `snap` so the value settles exactly (and the animation can
 * go inactive) instead of asymptoting forever.
 */
function ease(
  cur: number,
  tgt: number,
  up: number,
  down: number,
  snap: number,
): number {
  const diff = tgt - cur;
  if (Math.abs(diff) < snap) return tgt;
  return cur + diff * (diff > 0 ? up : down);
}

export const useSpinnerFrame = (): string =>
  DOTS_FRAMES[useAnimationTick(true) % DOTS_FRAMES.length]!;

export const useBouncingBall = (): string =>
  BALL_FRAMES[useAnimationTick(true) % BALL_FRAMES.length]!;

/**
 * Drop-in 2-cell rotating spinner. Use this anywhere a static refresh
 * glyph used to live — the row trailer, the list status marker, the
 * help-overlay legend, etc. Render as a direct child of a `<box>`,
 * not inside another `<text>`. Mount it only while spinning is the
 * message: a mounted spinner keeps the shared ticker (and therefore
 * ~10fps of repaints) alive.
 */
export function Spinner({ fg }: { fg: string }) {
  const frame = useSpinnerFrame();
  return <text fg={fg}>{frame}</text>;
}

type WaveAnim = {
  /** Eased strip width, fractional between ticks. */
  width: number;
  /** Eased 0..1 height envelope. */
  amp: number;
  /** Local phase counter driving the traveling-wave motion. */
  phase: number;
};

const WAVE_SETTLED: WaveAnim = { width: 0, amp: 0, phase: 0 };

/**
 * Header refresh indicator: a strip of `count` cells (capped at
 * {@link MAX_WAVE_WIDTH}) showing one shared traveling wave. Cell `i`
 * renders the wave frame at `phase + i`, so the cells read as a single
 * connected undulation flowing across the strip. Renders nothing when
 * `count` is 0; the wave keeps flowing as the count drains, so a refresh
 * reads as a live, shrinking ribbon rather than a flickering number. A
 * 0..1 height envelope ramps the cells up from the floor on appearance
 * and back down on drain. Always mounted in the title bar, so the
 * animation subscription is gated on having anything to show — at rest
 * this component costs nothing.
 */
export function RefreshWave({ count, fg }: { count: number; fg: string }) {
  const target = Math.min(Math.max(count, 0), MAX_WAVE_WIDTH);
  const [anim, setAnim] = useState<WaveAnim>(WAVE_SETTLED);
  // Read through a ref inside the tick callback so a count change
  // mid-subscription steers the easing without re-subscribing.
  const targetRef = useRef(target);
  targetRef.current = target;
  const active = target > 0 || anim.width > 0 || anim.amp > 0;
  useEffect(() => {
    if (!active) return;
    return ticker.subscribe(() => {
      setAnim((a) => {
        const width = ease(
          a.width,
          targetRef.current,
          WAVE_GROW_ALPHA,
          WAVE_SHRINK_ALPHA,
          0.5,
        );
        const amp = ease(
          a.amp,
          width > 0 ? 1 : 0,
          WAVE_AMP_ALPHA,
          WAVE_AMP_ALPHA,
          WAVE_AMP_SNAP,
        );
        if (width === 0 && amp === 0) return WAVE_SETTLED;
        return { width, amp, phase: a.phase + 1 };
      });
    });
  }, [active]);
  const width = Math.round(anim.width);
  if (width <= 0) return null;
  const len = WAVE_FRAMES.length;
  let s = "";
  for (let i = 0; i < width; i++) {
    const full = WAVE_FRAMES.charCodeAt((anim.phase + i) % len) - BLOCK_BASE; // 1..8
    const level = Math.round(full * anim.amp); // 0..8; 0 reads as flat floor
    s += level <= 0 ? " " : String.fromCharCode(BLOCK_BASE + level);
  }
  return <text fg={fg}>{s}</text>;
}
