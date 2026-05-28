/**
 * Opt-in event-loop-lag probe. Arms only when `WT_PERF` is set in the
 * environment. Samples the event loop on a fixed interval and logs
 * whenever a tick lands later than the threshold — meaning the single
 * JS thread was blocked by synchronous work for that long.
 *
 * This is the honest metric for the "j/k feels laggy during a refresh"
 * symptom: a blocked tick is a frame the TUI couldn't paint and a
 * keypress it couldn't read. Run `WT_PERF=1 bun src/main.ts` and grep
 * the daily app log for `event-loop blocked` to compare before/after.
 */
import { createLogger } from "./logger.ts";

const log = createLogger("[perf]");
const SAMPLE_MS = 100;
const LAG_THRESHOLD_MS = 50;

export function startLoopLagProbe(): () => void {
  if (!process.env.WT_PERF) return () => {};
  let last = performance.now();
  const timer = setInterval(() => {
    const now = performance.now();
    const lag = now - last - SAMPLE_MS;
    last = now;
    if (lag > LAG_THRESHOLD_MS) {
      log.warn("event-loop blocked", { lagMs: Math.round(lag) });
    }
  }, SAMPLE_MS);
  log.info("loop-lag probe armed", {
    sampleMs: SAMPLE_MS,
    thresholdMs: LAG_THRESHOLD_MS,
  });
  return () => clearInterval(timer);
}
