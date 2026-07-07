/**
 * Main-thread client for the Codex activity-pane event poller.
 *
 * The worker does the synchronous rollout-tree scan, file reads, and
 * JSON parsing. This module only samples the current active Codex
 * slots from the query cache, posts them to the worker, and forwards
 * worker-emitted event records through the normal logger so file +
 * activity-pane output stay unchanged.
 */
import { createLogger } from "../logger.ts";

import type {
  ActiveCodexSlug,
  CodexEventsWorkerMessage,
  CodexEventsWorkerResult,
} from "./codex-events-protocol.ts";

export type { ActiveCodexSlug };

const log = createLogger("[codex]");
const POLL_INTERVAL_MS = 2_500;

function post(worker: Worker, msg: CodexEventsWorkerMessage): void {
  worker.postMessage(msg);
}

function emit(result: CodexEventsWorkerResult): void {
  if (result.type === "warn") {
    log.warn("worker poll failed", { err: result.message });
    return;
  }
  for (const event of result.events) {
    log.event[event.level](event.text);
  }
}

/**
 * Start polling codex rollouts for active slots.
 *
 * @param getActiveSlugs - Called on every tick; must return the current
 *   list of active codex tmux slots (slug + worktree path). The caller
 *   should keep this cheap (a Map lookup, not a scan).
 * @returns A cleanup function that stops the interval and terminates
 *   the worker. Call it during TUI shutdown.
 */
export function startCodexEventPolling(
  getActiveSlugs: () => ReadonlyArray<ActiveCodexSlug>,
): () => void {
  const worker = new Worker(new URL("./codex-events-worker.ts", import.meta.url).href);
  let disposed = false;
  let inFlight = false;

  worker.addEventListener("message", (event: MessageEvent) => {
    inFlight = false;
    if (disposed) return;
    emit(event.data as CodexEventsWorkerResult);
  });
  worker.addEventListener("error", (event) => {
    inFlight = false;
    if (disposed) return;
    log.warn("worker error", { err: event.message });
  });
  worker.addEventListener("close", () => {
    inFlight = false;
    if (disposed) return;
    log.warn("worker exited");
  });
  // Idle worker should not keep wt alive during shutdown.
  (worker as Worker & { unref?: () => void }).unref?.();

  const tick = (): void => {
    if (disposed || inFlight) return;
    const active = getActiveSlugs();
    if (active.length === 0) {
      post(worker, { type: "poll", active });
      return;
    }
    inFlight = true;
    post(worker, { type: "poll", active });
  };

  const handle = setInterval(() => {
    try {
      tick();
    } catch (err) {
      inFlight = false;
      log.warn("poll tick failed", { err: String(err) });
    }
  }, POLL_INTERVAL_MS);

  return () => {
    disposed = true;
    clearInterval(handle);
    try {
      post(worker, { type: "stop" });
    } catch {
      // terminating below
    }
    try {
      worker.terminate();
    } catch {
      // already gone
    }
  };
}
