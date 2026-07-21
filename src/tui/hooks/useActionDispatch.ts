/**
 * Action launch + completion dispatch, extracted from `app.tsx`.
 *
 * Owns the two halves of the custom-action lifecycle that must share
 * state (the `pendingArgs` map):
 *
 *  - `launchAction` — guard, render vars, and start (or inject) a run.
 *  - The action-registry subscriber — fans a finished run's `affects`
 *    tags out to the matching invalidation helpers and refines the
 *    arg-history label from the captured output.
 *
 * See rule (3) in the architecture block at the top of
 * `state/hooks.ts` for the `affects` contract.
 */
import { useEffect, useRef } from "react";

import {
  actionRegistry,
  applyVars,
  BUILTIN_ACTIONS,
  evaluateActionRequirements,
  type ActionDef,
  type ActionVars,
} from "../../core/actions.ts";
import { recordRun as recordHistoryRun } from "../../core/actions.ts";
import { config } from "../../core/config.ts";
import { getHarness, type HarnessId } from "../../core/harness/index.ts";
import { createLogger } from "../../core/logger.ts";
import { injectIntoSession } from "../../core/tmux.ts";
import { StatusKind } from "../../core/types.ts";

import {
  actionSkillPrefix,
  buildActionVars,
  extractLabel,
  launchBlockedReason,
} from "../app-helpers.ts";
import type { WorktreeRow } from "./useWorktreeRows.ts";
import { theme } from "../theme.ts";

export type ActionDispatchOpts = {
  rows: readonly WorktreeRow[];
  primaryHarness: HarnessId;
  toast: (message: string, color?: string, ms?: number) => void;
  /** Clear a worktree's output focus so auto-rules surface the run. */
  setFocus: (slug: string, patch: { focused: string | null }) => void;
  invalidateWorktree: (slug: string) => Promise<void>;
  refreshOrigin: () => Promise<void>;
  refreshGithub: () => Promise<void>;
  refreshStack: () => Promise<void>;
};

export type LaunchActionOpts = {
  /**
   * Fire keys of the automation dispatch launching this run. Stamped
   * into the headless run's meta.json so the automation ledger's boot
   * reconciliation can match a `dispatched` entry against a run that
   * really launched. Absent for manual launches.
   */
  autoFireKeys?: readonly string[];
};

/**
 * Did the launch actually hand work off? `launched: false` means a
 * guard declined it BEFORE anything ran (busy worktree, action already
 * running, unmet requirements, …) — for manual launches the toast is
 * the whole story, but the automations engine uses the distinction to
 * un-consume the fire instead of recording a run that never happened.
 * Session-target injections report `launched: true` at hand-off (the
 * paste is fire-and-forget by design).
 */
export type LaunchOutcome = { launched: boolean; reason?: string };

export function useActionDispatch(opts: ActionDispatchOpts): {
  launchAction: (
    slug: string,
    def: ActionDef | null,
    extras: string,
    arg?: string,
    launchOpts?: LaunchActionOpts,
  ) => Promise<LaunchOutcome>;
} {
  // Custom action effect dispatch — each action carries an `affects`
  // tag set captured at start time; on every transition from
  // `running` → terminal status, fan that out to the matching
  // invalidation helpers. The `handled` set keys on `slug@endedAt`
  // so a completion fires exactly once even when the registry
  // notifies for unrelated state churn afterwards.
  //
  // `handled` and the helper closures live in refs so the effect
  // subscribes exactly once at mount. The caller passes fresh helper
  // closures every render — without the ref indirection the deps
  // array would tear down + re-seed on every render, and a completion
  // that fires inside that window can be lost to the seed before
  // dispatch runs.
  const helpersRef = useRef({
    invalidateWorktree: opts.invalidateWorktree,
    refreshOrigin: opts.refreshOrigin,
    refreshGithub: opts.refreshGithub,
    refreshStack: opts.refreshStack,
  });
  helpersRef.current = {
    invalidateWorktree: opts.invalidateWorktree,
    refreshOrigin: opts.refreshOrigin,
    refreshGithub: opts.refreshGithub,
    refreshStack: opts.refreshStack,
  };
  const handledRef = useRef<Set<string>>(new Set());
  /**
   * Per-launch arg values, keyed by `${slug}/${actionId}`. Populated by
   * `launchAction` when an arg was supplied; consulted by the action-
   * registry subscriber once the matching run reaches a terminal status
   * to refine the just-written history entry via the def's
   * `label_extract` regex against the captured output. Cleared on
   * consumption — bounded by the number of concurrent in-flight runs.
   */
  const pendingArgs = useRef<Map<string, string>>(new Map());
  useEffect(() => {
    const handled = handledRef.current;
    // Seed once with already-finished runs so a fresh mount doesn't
    // re-fire dispatch for runs the previous mount already handled.
    // (Singleton registry survives across mounts; ref survives across
    // renders. The seed is a no-op on a clean process start.)
    for (const run of actionRegistry.getSnapshot().values()) {
      if (run.status !== "running" && run.endedAt !== undefined) {
        handled.add(`${run.slug}@${run.endedAt}`);
      }
    }
    return actionRegistry.subscribe(() => {
      for (const run of actionRegistry.getSnapshot().values()) {
        if (run.status === "running") continue;
        if (run.endedAt === undefined) continue;
        const key = `${run.slug}@${run.endedAt}`;
        if (handled.has(key)) continue;
        handled.add(key);
        const {
          invalidateWorktree: inv,
          refreshOrigin: ro,
          refreshGithub: rg,
          refreshStack: rs,
        } = helpersRef.current;
        for (const tag of run.affects) {
          switch (tag) {
            case "git":
              void ro();
              void inv(run.slug);
              // History-rewriting actions (rebase, modify, …) rewrite
              // commits under a fixed explicit parent, so the per-base
              // diff / sync queries need a re-run even though the parent
              // relationship is unchanged. `refreshStack` invalidates
              // those (see its doc in state/hooks.ts).
              void rs();
              break;
            case "github":
              void rg();
              break;
            default: {
              // Exhaustiveness check — a new EffectTag without a case
              // here would silently skip its invalidation, leaving the
              // UI stale after the action exits.
              const _exhaustive: never = tag;
              void _exhaustive;
            }
          }
        }
        // Arg-prompt history label refinement. Only fires for runs the
        // current TUI session launched with an `{{arg}}` value AND
        // succeeded. Looks up the def by actionId, then scans the
        // captured output with its `label_extract` regex (when set)
        // and (re)writes the history entry with the matched label.
        // No def, no regex, or no match → entry keeps the raw value;
        // graceful default.
        const argKey = `${run.slug}/${run.actionId}`;
        const argVal = pendingArgs.current.get(argKey);
        if (argVal !== undefined) {
          pendingArgs.current.delete(argKey);
          if (run.status === "succeeded") {
            const def =
              config.actions.find((d) => d.id === run.actionId) ??
              BUILTIN_ACTIONS.find((d) => d.id === run.actionId) ??
              null;
            const label = extractLabel(run.lines, def?.labelExtract ?? null);
            // Suppress redundant labels — when the regex captures the
            // same text the user typed (e.g. "Seeding company: <id>"
            // with no name resolution), recording it would render the
            // picker as `<id> · <id>`. Skip the update; the entry
            // keeps its `label: null` from launch-time and the picker
            // shows just the raw value.
            if (label && label !== argVal) {
              recordHistoryRun(run.actionId, argVal, label);
            }
          }
        }
      }
    });
  }, []);

  async function launchAction(
    slug: string,
    def: ActionDef | null,
    extras: string,
    arg?: string,
    launchOpts: LaunchActionOpts = {},
  ): Promise<LaunchOutcome> {
    const { rows, primaryHarness, toast, setFocus } = opts;
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row) {
      toast("worktree gone", theme.warn, 1500);
      return { launched: false, reason: "worktree gone" };
    }
    if (!def && !extras.trim()) {
      toast("prompt is empty", theme.warn, 1500);
      return { launched: false, reason: "prompt is empty" };
    }
    // Refuse if the worktree is being cleaned up (archived the instant a
    // destroy/clean dispatches, before the flock exists) or mid-destroy /
    // mid-init (flock held). The archived half matters: a clean/destroy
    // flips `row.archived` synchronously but the detached `_destroy`
    // child only grabs the flock a process-spawn later, so `lockStatus`
    // alone leaves a window where an action would launch into a directory
    // about to be `git worktree remove --force`d. `launchBlockedReason`
    // checks both — the same gate every session launch uses.
    const blocked = launchBlockedReason(row);
    if (blocked) {
      toast(`${slug} is ${blocked}`, theme.warn, 2000);
      return { launched: false, reason: `${slug} is ${blocked}` };
    }
    if (row.status.kind === StatusKind.Busy) {
      toast(`${slug} is busy`, theme.warn, 2000);
      return { launched: false, reason: `${slug} is busy` };
    }
    if (def) {
      const avail = evaluateActionRequirements(def.requires, {
        pr: row.pr,
        deployed: row.fields.deploy.data ?? false,
      });
      if (!avail.ok) {
        toast(`${def.name}: ${avail.reason}`, theme.warn, 2500);
        return { launched: false, reason: avail.reason };
      }
    }
    const baseVars = buildActionVars(row, actionSkillPrefix(def, primaryHarness));
    // `{{arg}}` substitution lives alongside the row-derived vars. The
    // value, when present, came from the action-arg picker; gets folded
    // in for both shell and claude actions (including session-target).
    const vars: ActionVars = arg ? { ...baseVars, arg } : baseVars;
    // Record the value used so the next picker open shows it at top.
    // Label is null here — the LABEL scan in the actionRegistry
    // subscriber refines it after the run finishes (if the script
    // emitted a marker line). Idempotent against re-runs of the same
    // value (LRU dedup).
    if (def && arg && def.id !== "__custom__") {
      recordHistoryRun(def.id, arg, null);
      pendingArgs.current.set(`${slug}/${def.id}`, arg);
    }
    // Session-target prompt actions bypass the headless `-p` runner and
    // type the prompt into the live primary F12 harness session (starting
    // it if needed). This follows the Shift+TAB-selected primary harness,
    // so actions like `/rabbit` land in Codex/OpenCode when that is the
    // row's default AI.
    // Fire-and-forget: there's no run to track or focus, so we just log
    // progress to the activity pane. The cold-start path can take a few
    // seconds, hence the immediate "sending…" toast.
    if (def && def.kind === "claude" && def.target === "session") {
      const renderedPrompt = applyVars(def.prompt, vars);
      const trimmedExtras = applyVars(extras, vars).trim();
      const fullPrompt = trimmedExtras
        ? `${renderedPrompt}\n\n${trimmedExtras}`
        : renderedPrompt;
      const sessionLog = createLogger(slug);
      const harness = getHarness(primaryHarness);
      sessionLog.event.info(`${def.name} → live ${harness.label} session`);
      toast(`sending ${def.name} to session…`, theme.info, 2000);
      void injectIntoSession({
        slug,
        cwd: row.wt.path,
        harnessId: primaryHarness,
        text: fullPrompt,
      }).then(
        (res) => {
          if (res.ok) {
            sessionLog.event.ok(
              res.coldStarted
                ? `started ${harness.label} session and sent ${def.name}`
                : `sent ${def.name} to ${harness.label} session`,
            );
          } else {
            sessionLog.event.err(`inject failed: ${res.reason}`);
            toast(`inject failed: ${res.reason}`, theme.err, 3000);
          }
        },
      );
      return { launched: true };
    }
    const result = def
      ? await actionRegistry.start(
          def,
          slug,
          row.wt.path,
          extras,
          vars,
          primaryHarness,
          { autoFireKeys: launchOpts.autoFireKeys },
        )
      : await actionRegistry.startCustom(
          slug,
          row.wt.path,
          extras,
          vars,
          primaryHarness,
        );
    if (!result.ok) {
      toast(`action: ${result.reason}`, theme.err, 3000);
      return { launched: false, reason: result.reason };
    }
    // Clear this worktree's focus so the auto-rules surface the
    // just-launched action.
    setFocus(slug, { focused: null });
    toast(`launched ${result.run.actionName}`, theme.info, 2000);
    return { launched: true };
  }

  return { launchAction };
}
