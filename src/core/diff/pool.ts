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
 *
 * Worker death is a first-class outcome: each `Pending` records its
 * owning worker, and an `error`/`close` event rejects exactly that
 * worker's in-flight jobs (so their queryFn surfaces an error instead
 * of hanging forever) and drops the dead worker. The pool refills
 * lazily on the next dispatch once it empties — no eager respawn, which
 * would tight-loop if a worker crashed at import.
 */
import { chainSignal } from "../proc.ts";

import type { DiffContext } from "./index.ts";
import type { DiffJobMessage, DiffJobResult } from "./protocol.ts";

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
  /** The worker this job was dispatched to, so a worker-death handler
   *  can reject exactly the jobs that died with it. */
  worker: Worker;
};

let workers: Worker[] = [];
const pending = new Map<number, Pending>();
let nextId = 1;
let nextWorkerIndex = 0;
let disposed = false;

/** Typed send so a `protocol.ts` field rename is a compile error rather
 *  than a runtime surprise (`postMessage` itself is untyped). */
function post(worker: Worker, msg: DiffJobMessage): void {
  worker.postMessage(msg);
}

function handleMessage(event: MessageEvent): void {
  // The wire is untyped; this is the one honest assertion at the boundary.
  const msg = event.data as DiffJobResult;
  const entry = pending.get(msg.id);
  if (!entry) return; // cancelled, disposed, or already settled
  pending.delete(msg.id);
  entry.cleanup();
  if (msg.type === "result") entry.resolve(msg.ctx);
  else entry.reject(new Error(msg.message));
}

/**
 * Reject every in-flight job that was dispatched to `worker` (it died),
 * then drop it from the pool. No eager respawn — `ensurePool` refills
 * when the pool next empties, so a worker that crashes at import can't
 * drive a tight respawn loop.
 */
function failWorker(worker: Worker, reason: string): void {
  for (const [id, entry] of pending) {
    if (entry.worker !== worker) continue;
    pending.delete(id);
    entry.cleanup();
    entry.reject(new Error(`diff worker died (${reason})`));
  }
  const idx = workers.indexOf(worker);
  if (idx !== -1) workers.splice(idx, 1);
  try {
    worker.terminate();
  } catch {
    // already gone
  }
}

function spawnWorker(): Worker {
  const worker = new Worker(new URL("./diff-worker.ts", import.meta.url).href);
  worker.addEventListener("message", handleMessage);
  worker.addEventListener("error", (event) => {
    if (disposed) return;
    failWorker(worker, event.message || "error");
  });
  worker.addEventListener("close", () => {
    if (disposed) return;
    failWorker(worker, "exited");
  });
  // Idle diff workers shouldn't keep the process alive on exit.
  (worker as Worker & { unref?: () => void }).unref?.();
  workers.push(worker);
  return worker;
}

function ensurePool(): void {
  if (workers.length > 0) return;
  try {
    for (let i = 0; i < POOL_SIZE; i++) spawnWorker();
  } catch (err) {
    // Partial spawn — tear down whatever came up so the next call
    // retries from a clean slate instead of dispatching onto a
    // half-built pool.
    for (const worker of workers) {
      try {
        worker.terminate();
      } catch {
        // already gone
      }
    }
    workers = [];
    throw err;
  }
}

export function buildDiffContextViaPool(
  wtPath: string,
  base: string,
  signal?: AbortSignal,
): Promise<DiffContext | null> {
  // A refetch can fire during shutdown, after disposeDiffPool() emptied
  // the pool but before the query cache is cleared. Don't re-spawn a
  // pool that nothing will ever tear down again — reject as a cancel.
  if (disposed) {
    return Promise.reject(new DOMException("diff pool disposed", "AbortError"));
  }
  ensurePool();
  const id = nextId++;
  const worker = workers[nextWorkerIndex++ % workers.length]!;
  return new Promise<DiffContext | null>((resolve, reject) => {
    pending.set(id, { resolve, reject, cleanup: () => {}, worker });
    if (signal) {
      const cleanup = chainSignal(signal, () => {
        post(worker, { type: "cancel", id });
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
    if (pending.has(id)) post(worker, { type: "run", id, wtPath, base });
  });
}

export function disposeDiffPool(): void {
  disposed = true;
  for (const worker of workers) {
    try {
      worker.terminate();
    } catch {
      // already gone
    }
  }
  workers = [];
  nextWorkerIndex = 0;
  for (const entry of pending.values()) {
    entry.cleanup();
    // Reject as an abort, not a generic error: TanStack treats an
    // AbortError as a cancellation (no error state, no retry) rather
    // than a query failure during teardown.
    entry.reject(new DOMException("diff pool disposed", "AbortError"));
  }
  pending.clear();
}
