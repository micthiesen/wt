import { useEffect, useRef, useState } from "react";

import { createLogger } from "../../core/logger.ts";
import { latestLogFor } from "../../core/logs.ts";
import { streamLines } from "../../core/proc.ts";
import { StatusKind } from "../../core/types.ts";
import type { WorktreeRow } from "./useWorktreeRows.ts";

type Tail = {
  proc: Bun.Subprocess;
  controller: AbortController;
};

/**
 * Tail log files for any worktree currently running a background job.
 * Each tail's lines are funneled into the global event log under the
 * worktree's slug. Returns the set of slugs currently being tailed so
 * callers can render a visual indicator.
 */
export function useLogTails(rows: WorktreeRow[]): Set<string> {
  const tails = useRef<Map<string, Tail>>(new Map());
  const [active, setActive] = useState<Set<string>>(() => new Set());

  useEffect(() => {
    const wanted = new Map<string, string>();
    for (const r of rows) {
      if (r.status.kind !== StatusKind.Busy) continue;
      const log = latestLogFor(r.wt.slug);
      if (log) wanted.set(r.wt.slug, log);
    }

    // Stop tails that are no longer wanted.
    for (const [slug, tail] of tails.current) {
      if (!wanted.has(slug)) {
        tail.controller.abort();
        tails.current.delete(slug);
      }
    }

    // Start new tails.
    for (const [slug, logPath] of wanted) {
      if (tails.current.has(slug)) continue;
      const controller = new AbortController();
      const log = createLogger(slug);
      try {
        const proc = Bun.spawn(["tail", "-n", "50", "-F", logPath], {
          stdin: "ignore",
          stdout: "pipe",
          stderr: "pipe",
          signal: controller.signal,
        });
        tails.current.set(slug, { proc, controller });
        log.event.info(`tailing ${logPath}`);
        void pumpTail(proc, slug);
      } catch (err) {
        log.event.err(`tail failed: ${err instanceof Error ? err.message : String(err)}`);
        log.error(err instanceof Error ? err : String(err));
      }
    }

    setActive((prev) => {
      const next = new Set(wanted.keys());
      if (prev.size === next.size && [...prev].every((s) => next.has(s))) return prev;
      return next;
    });
  }, [rows]);

  // Hard-stop on unmount so the TUI can exit cleanly.
  useEffect(() => {
    return () => {
      for (const [, tail] of tails.current) tail.controller.abort();
      tails.current.clear();
    };
  }, []);

  return active;
}

async function pumpTail(proc: Bun.Subprocess, slug: string): Promise<void> {
  const stdout = proc.stdout as ReadableStream<Uint8Array> | undefined;
  if (!stdout) return;
  const log = createLogger(slug);
  try {
    await streamLines(stdout, (line) => {
      if (line.trim()) log.event.dim(line);
    });
  } catch {
    // tail was aborted via AbortController — normal shutdown path.
  }
}
