/**
 * Wire protocol between the main thread and the diff-compaction Worker
 * pool. Pure types — imported by both `pool.ts` (main) and
 * `diff-worker.ts` (worker), so it carries no runtime code that would
 * drag the main-thread pool into the worker bundle or vice versa.
 */
import type { DiffContext } from "./index.ts";

/** Compute the diff context for one (worktree, base) pair. */
export type DiffJobRequest = {
  type: "run";
  id: number;
  wtPath: string;
  base: string;
};

/** Cancel an in-flight job (superseded query / unmounted observer). */
export type DiffJobCancel = { type: "cancel"; id: number };

export type DiffJobMessage = DiffJobRequest | DiffJobCancel;

export type DiffJobResult =
  | { type: "result"; id: number; ctx: DiffContext | null }
  | { type: "error"; id: number; message: string };
