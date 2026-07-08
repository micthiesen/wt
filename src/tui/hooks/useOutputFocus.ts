import { useEffect, useMemo, useState } from "react";

import type { ActionRun } from "../../core/actions.ts";
import { createLogger } from "../../core/logger.ts";
import {
  type Output,
  actionOutputId,
  destroyOutputId,
  eventsOutputId,
  outputsForSlug,
  sessionOutputId,
} from "../../core/outputs.ts";
import { StatusKind } from "../../core/types.ts";
import { useOutputs } from "./useOutputs.ts";
import type { ActiveSessionGlyph } from "./useHarnessSessions.ts";
import type { WorktreeRow } from "./useWorktreeRows.ts";

type SlugFocus = { focused: string | null };
const NO_ROW_KEY = "__no_row__";
const EMPTY_FOCUS: SlugFocus = { focused: null };

type Args = {
  rows: readonly WorktreeRow[];
  currentSlug: string | undefined;
  currentRun: ActionRun | null;
  showActionViewer: boolean;
  claudeSessionsBySlug: ReadonlyMap<string, ReadonlyArray<string | null>>;
  activeSessionBySlug: ReadonlyMap<string, ActiveSessionGlyph>;
};

export function useOutputFocus({
  rows,
  currentSlug,
  currentRun,
  showActionViewer,
  claudeSessionsBySlug,
  activeSessionBySlug,
}: Args) {
  const [slugFocus, setSlugFocus] = useState<Record<string, SlugFocus>>({});

  const destroyingSlugs = useMemo(
    () =>
      rows
        .filter(
          (r) => r.status.kind === StatusKind.Busy && r.status.op === "remove",
        )
        .map((r) => r.wt.slug),
    [rows],
  );
  const outputs = useOutputs({ destroyingSlugs });
  const focusKey = currentSlug ?? NO_ROW_KEY;
  const focusBucket = slugFocus[focusKey] ?? EMPTY_FOCUS;
  const visibleOutputs = useMemo(
    () => outputsForSlug(outputs, currentSlug ?? null),
    [outputs, currentSlug],
  );
  const isDestroying =
    currentSlug !== undefined && destroyingSlugs.includes(currentSlug);

  const autoOutputId = useMemo<string>(() => {
    if (currentSlug && isDestroying) {
      return destroyOutputId(currentSlug);
    }
    if (currentSlug && currentRun && showActionViewer) {
      return actionOutputId(currentSlug, currentRun.startedAt);
    }
    if (currentSlug) {
      const active = activeSessionBySlug.get(currentSlug);
      if (active && active.harnessId !== "claude") {
        const id = sessionOutputId(currentSlug, active.harnessId);
        if (visibleOutputs.some((o) => o.id === id)) return id;
      }
      const liveNames = claudeSessionsBySlug.get(currentSlug);
      if (liveNames && liveNames.length > 0) {
        if (liveNames.includes(null)) {
          return sessionOutputId(currentSlug, "claude", null);
        }
        const liveClaude = visibleOutputs.find(
          (o) =>
            o.kind === "session" &&
            o.sessionKind === "claude" &&
            o.sessionName !== null,
        );
        if (liveClaude) return liveClaude.id;
      }
    }
    return eventsOutputId();
  }, [
    currentSlug,
    isDestroying,
    currentRun?.startedAt,
    showActionViewer,
    claudeSessionsBySlug,
    activeSessionBySlug,
    visibleOutputs,
  ]);

  const desiredOutputId = focusBucket.focused ?? autoOutputId;
  const displayedOutput: Output =
    visibleOutputs.find((o) => o.id === desiredOutputId) ??
    visibleOutputs.find((o) => o.id === autoOutputId) ??
    visibleOutputs[0]!;

  useEffect(() => {
    const liveSlugs = new Set<string>([NO_ROW_KEY]);
    for (const r of rows) liveSlugs.add(r.wt.slug);
    const liveOutputIds = new Set<string>();
    for (const o of outputs) liveOutputIds.add(o.id);

    let changed = false;
    const next: Record<string, SlugFocus> = {};
    const evictedSlugs: string[] = [];
    for (const [key, bucket] of Object.entries(slugFocus)) {
      if (!liveSlugs.has(key)) {
        if (bucket.focused !== null) evictedSlugs.push(key);
        changed = true;
        continue;
      }
      const focused =
        bucket.focused && liveOutputIds.has(bucket.focused)
          ? bucket.focused
          : null;
      if (focused !== bucket.focused) changed = true;
      if (focused === null) {
        changed = true;
        continue;
      }
      next[key] = { focused };
    }

    if (!changed) return;
    for (const key of evictedSlugs) {
      createLogger("[app]").event.dim(`dropped output state for ${key} (worktree gone)`);
    }
    setSlugFocus(next);
  }, [outputs, rows, slugFocus]);

  function setFocus(slug: string | null, patch: Partial<SlugFocus>): void {
    const key = slug ?? NO_ROW_KEY;
    setSlugFocus((prev) => {
      const cur = prev[key] ?? EMPTY_FOCUS;
      const next = { ...cur, ...patch };
      if (next.focused === null) {
        if (!(key in prev)) return prev;
        const { [key]: _, ...rest } = prev;
        return rest;
      }
      return { ...prev, [key]: next };
    });
  }

  return {
    visibleOutputs,
    displayedOutput,
    focusedOutputId: focusBucket.focused,
    setFocus,
  };
}
