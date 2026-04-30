import { useEffect, useState } from "react";
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

/** Cell width of {@link Spinner} — useful when sizing layout slots. */
export const SPINNER_WIDTH = 2;

function useFrameLoop(frames: readonly string[], frameMs: number): string {
  const cycleMs = frameMs * frames.length;
  const [idx, setIdx] = useState(0);
  const timeline = useTimeline({ duration: cycleMs, loop: true });
  // Register the animation once on mount. `useTimeline` returns a fresh
  // (unregistered) Timeline on every render — only the one captured here
  // at mount is wired into the engine, so a stable empty dep list is
  // intentional.
  useEffect(() => {
    const target = { t: 0 };
    timeline.add(target, {
      t: frames.length,
      duration: cycleMs,
      ease: "linear",
      onUpdate: () => {
        const next = Math.floor(target.t) % frames.length;
        setIdx((prev) => (prev === next ? prev : next));
      },
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  return frames[idx]!;
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
