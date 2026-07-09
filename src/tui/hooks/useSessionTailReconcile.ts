import { useEffect, useRef } from "react";

import { config } from "../../core/config.ts";
import {
  harnessTailRegistry,
  type LiveHarnessSlot,
} from "../../core/harness/harness-tail.ts";
import { createLogger } from "../../core/logger.ts";
import {
  sessionTailRegistry,
  type LiveSessionDesc,
} from "../../core/harness/claude/tail.ts";
import { shellTailRegistry } from "../../core/shell-tail.ts";
import { diffCommandUsesBase, killDiffSession } from "../../core/tmux.ts";
import { resolveDiffBase } from "../app-helpers.ts";
import { SESSION_SLOTS } from "../session-slots.ts";
import type { WorktreeRow } from "./useWorktreeRows.ts";

type Args = {
  rows: readonly WorktreeRow[];
  claudeSessionsBySlug: ReadonlyMap<string, ReadonlyArray<string | null>>;
  activeShellSessions: ReadonlySet<string>;
  activeCodexSessions: ReadonlySet<string>;
  activeOpencodeSessions: ReadonlySet<string>;
  activeDiffSessions: ReadonlySet<string>;
  refreshTmuxSessions: () => Promise<unknown>;
};

export function useSessionTailReconcile({
  rows,
  claudeSessionsBySlug,
  activeShellSessions,
  activeCodexSessions,
  activeOpencodeSessions,
  activeDiffSessions,
  refreshTmuxSessions,
}: Args): void {
  // Reconcile session tailers against the live (slug, name) set so the
  // jsonl-watch lifecycle tracks the daemon.
  useEffect(() => {
    const pathBySlug = new Map<string, string>();
    for (const slot of SESSION_SLOTS) pathBySlug.set(slot.slug, slot.path);
    for (const r of rows) pathBySlug.set(r.wt.slug, r.wt.path);
    const live: LiveSessionDesc[] = [];
    for (const [slug, names] of claudeSessionsBySlug) {
      const wtPath = pathBySlug.get(slug);
      if (!wtPath) continue;
      for (const name of names) live.push({ slug, name, wtPath });
    }
    sessionTailRegistry.reconcile(live);
  }, [rows, claudeSessionsBySlug]);

  useEffect(() => {
    const live = new Set<string>();
    for (const r of rows) {
      if (activeShellSessions.has(r.wt.slug)) live.add(r.wt.slug);
    }
    shellTailRegistry.reconcile(live);
  }, [rows, activeShellSessions]);

  useEffect(() => {
    const pathBySlug = new Map<string, string>();
    for (const slot of SESSION_SLOTS) pathBySlug.set(slot.slug, slot.path);
    for (const r of rows) pathBySlug.set(r.wt.slug, r.wt.path);
    const live: LiveHarnessSlot[] = [];
    const add = (slugs: ReadonlySet<string>, harnessId: "codex" | "opencode") => {
      for (const slug of slugs) {
        const wtPath = pathBySlug.get(slug);
        if (wtPath) live.push({ slug, wtPath, harnessId });
      }
    };
    add(activeCodexSessions, "codex");
    add(activeOpencodeSessions, "opencode");
    harnessTailRegistry.reconcile(live);
  }, [rows, activeCodexSessions, activeOpencodeSessions]);

  const lastDiffBase = useRef<Map<string, string>>(new Map());
  useEffect(() => {
    if (!diffCommandUsesBase(config.diff.command)) return;
    const seen = new Set<string>();
    for (const r of rows) {
      const slug = r.wt.slug;
      seen.add(slug);
      const next = resolveDiffBase(r);
      const prev = lastDiffBase.current.get(slug);
      lastDiffBase.current.set(slug, next);
      if (prev === undefined || prev === next) continue;
      if (!activeDiffSessions.has(slug)) continue;
      const log = createLogger(slug);
      log.event.info(`diff base changed (${prev} -> ${next}); killing diff session`);
      void (async () => {
        await killDiffSession(slug);
        await refreshTmuxSessions();
      })();
    }
    for (const slug of [...lastDiffBase.current.keys()]) {
      if (!seen.has(slug)) lastDiffBase.current.delete(slug);
    }
  }, [rows, activeDiffSessions, refreshTmuxSessions]);
}
