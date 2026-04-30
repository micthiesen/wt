import { useEffect, useState } from "react";
import { useTimeline } from "@opentui/react";

/**
 * Classic braille "dots" spinner — same frames ora/listr/npm use, and the
 * Lilex Nerd Font Mono renders each cleanly in one cell.
 *
 * Uses `useTimeline` so the tick is wired into OpenTUI's renderer frame
 * loop (raw `setInterval` + `setState` updates don't reliably propagate
 * through the OpenTUI React reconciler outside the render loop).
 */
const FRAMES = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"] as const;
const FRAME_MS = 80;
const CYCLE_MS = FRAME_MS * FRAMES.length;

export function useSpinnerFrame(): string {
  const [idx, setIdx] = useState(0);
  const timeline = useTimeline({ duration: CYCLE_MS, loop: true });
  // Register the animation once on mount. `useTimeline` returns a fresh
  // (unregistered) Timeline on every render — only the one captured here
  // at mount is wired into the engine, so a stable empty dep list is
  // intentional.
  useEffect(() => {
    const target = { t: 0 };
    timeline.add(target, {
      t: FRAMES.length,
      duration: CYCLE_MS,
      ease: "linear",
      onUpdate: () => {
        const next = Math.floor(target.t) % FRAMES.length;
        setIdx((prev) => (prev === next ? prev : next));
      },
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  return FRAMES[idx]!;
}
