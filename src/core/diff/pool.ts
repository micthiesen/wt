/**
 * Main-thread client for the diff-compaction Worker pool. Dispatches
 * `buildDiffContext` jobs round-robin across a small fixed pool so the
 * git diff + parse + fit + SHA-256 work never blocks the TUI render
 * thread (the cause of j/k lag during a refresh). The queryFn's
 * `AbortSignal` is forwarded to the worker as a `cancel` message so a
 * superseded query stops burning a git invocation in the background.
 *
 * The pool spawns lazily on first use and is torn down via
 * `disposeDiffPool()` from the TUI shutdown path. Job ids are
 * monotonic and never reused, so a late reply for a cancelled job
 * simply finds no pending entry and is dropped.
 */
import { chainSignal } from "../proc.ts";

import type { DiffContext } from "./index.ts";
import type { DiffJobResult } from "./protocol.ts";

// Keep git I/O concurrency reasonable without spawning a worker per
// core. A handful is plenty — diffs feed background AI summaries, not
// the immediate UI, so throughput matters less than keeping the main
// thread free.
const POOL_SIZE = Math.max(
  2,
  Math.min(4, (navigator.hardwareConcurrency || 4) - 1),
);

type Pending = {
  resolve: (ctx: DiffContext | null) => void;
  reject: (err: Error) => void;
  cleanup: () => void;
};

let workers: Worker[] = [];
const pending = new Map<number, Pending>();
let nextId = 1;
let rr = 0;

function handleMessage(event: MessageEvent<DiffJobResult>): void {
  const msg = event.data;
  const entry = pending.get(msg.id);
  if (!entry) return; // cancelled, disposed, or already settled
  pending.delete(msg.id);
  entry.cleanup();
  if (msg.type === "result") entry.resolve(msg.ctx);
  else entry.reject(new Error(msg.message));
}

function ensurePool(): void {
  if (workers.length > 0) return;
  for (let i = 0; i < POOL_SIZE; i++) {
    const worker = new Worker(new URL("./diff-worker.ts", import.meta.url).href);
    worker.addEventListener(
      "message",
      handleMessage as (e: MessageEvent) => void,
    );
    // Idle diff workers shouldn't keep the process alive on exit.
    (worker as { unref?: () => void }).unref?.();
    workers.push(worker);
  }
}

export function buildDiffContextViaPool(
  wtPath: string,
  base: string,
  signal?: AbortSignal,
): Promise<DiffContext | null> {
  ensurePool();
  const id = nextId++;
  const worker = workers[rr++ % workers.length]!;
  return new Promise<DiffContext | null>((resolve, reject) => {
    pending.set(id, { resolve, reject, cleanup: () => {} });
    if (signal) {
      const cleanup = chainSignal(signal, () => {
        worker.postMessage({ type: "cancel", id });
        const entry = pending.get(id);
        if (entry) {
          pending.delete(id);
          entry.reject(new DOMException("Aborted", "AbortError"));
        }
      });
      const entry = pending.get(id);
      if (entry) entry.cleanup = cleanup;
    }
    // The signal may have already aborted synchronously above and
    // settled the promise — only dispatch the job if it's still live.
    if (pending.has(id)) worker.postMessage({ type: "run", id, wtPath, base });
  });
}

export function disposeDiffPool(): void {
  for (const worker of workers) {
    try {
      worker.terminate();
    } catch {
      // already gone
    }
  }
  workers = [];
  for (const entry of pending.values()) {
    entry.cleanup();
    entry.reject(new Error("diff pool disposed"));
  }
  pending.clear();
}
