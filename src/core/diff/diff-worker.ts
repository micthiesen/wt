/**
 * Worker entry for the diff-compaction pool. `buildDiffContext` runs
 * here — the `git diff` subprocess plus the *synchronous* parse / fit /
 * SHA-256 burst that used to land on the TUI's single render thread on
 * every refresh. Doing it off-thread is what stops j/k input from
 * queueing behind a refresh. See `pool.ts` for the main-side client.
 *
 * State is just the in-flight `AbortController`s, keyed by job id. A
 * cancel message aborts the controller, which `run` (down in
 * `buildDiffContext`) turns into a SIGTERM of the git subprocess —
 * the kill happens there, not here.
 */
import { buildDiffContext } from "./index.ts";
import type { DiffJobMessage, DiffJobResult } from "./protocol.ts";

declare var self: Worker;

const controllers = new Map<number, AbortController>();

function reply(msg: DiffJobResult): void {
  postMessage(msg);
}

self.onmessage = (event: MessageEvent<DiffJobMessage>) => {
  const msg = event.data;
  if (msg.type === "cancel") {
    controllers.get(msg.id)?.abort();
    controllers.delete(msg.id);
    return;
  }
  const ac = new AbortController();
  controllers.set(msg.id, ac);
  // Guard the dispatch itself: if `buildDiffContext` throws
  // *synchronously* (before returning a promise) the .then/.catch below
  // never attach, the throw escapes onmessage, and the job would strand
  // with no reply. Reply an error synchronously instead.
  try {
    buildDiffContext(msg.wtPath, msg.base, ac.signal)
      .then((ctx) => {
        controllers.delete(msg.id);
        reply({ type: "result", id: msg.id, ctx });
      })
      .catch((err: unknown) => {
        controllers.delete(msg.id);
        reply({
          type: "error",
          id: msg.id,
          message: err instanceof Error ? err.message : String(err),
        });
      });
  } catch (err: unknown) {
    controllers.delete(msg.id);
    reply({
      type: "error",
      id: msg.id,
      message: err instanceof Error ? err.message : String(err),
    });
  }
};
