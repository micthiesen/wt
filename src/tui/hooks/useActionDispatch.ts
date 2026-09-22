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
import { useQueryClient } from "@tanstack/react-query";
import { Effect } from "effect";

import {
  actionRegistry,
  applyVars,
  ALL_BUILTIN_ACTIONS,
  evaluateActionRequirements,
  type ActionDef,
  type ActionVars,
} from "../../core/actions.ts";
import { recordRun as recordHistoryRun } from "../../core/actions.ts";
import { config } from "../../core/config.ts";
import { trackIssueStatusAction } from "../../state/issue-status.ts";
import { operationErrors } from "../../core/errors.ts";
import type { HarnessId } from "../../core/harness/index.ts";
import { sendAgentMessage } from "../../core/harness/agent-routing.ts";
import { sendAgentCompact, type CompactResult } from "../../core/harness/compact.ts";
import { createLogger } from "../../core/logger.ts";
import { sendWorktreeMessage } from "../../core/worktree-executor.ts";
import { forkReported } from "../effect-boundary.ts";
import { StatusKind } from "../../core/types.ts";
import {
  isRemoteWorktreeTarget,
  localWorktreeTarget,
  type WorktreeTarget,
} from "../../core/worktree-target.ts";
import { MANAGER_SLOT, SESSION_SLOTS } from "../sessions/slots.ts";

import {
  actionSkillPrefix,
  extractLabel,
} from "../app-helpers.ts";
import {
  actionSubjectBlockedReason,
  actionSubjectVars,
  type ActionSubjectResolver,
} from "../action-subject.ts";
import type { WorktreeRow } from "./useWorktreeRows.ts";
import { theme } from "../theme.ts";

export type ActionDispatchOpts = {
  rows: readonly WorktreeRow[];
  actionSubjectFor: ActionSubjectResolver;
  primaryHarness: HarnessId;
  toast: (message: string, color?: string, ms?: number) => void;
  /** Clear a worktree's output focus so auto-rules surface the run. */
  setFocus: (slug: string, patch: { focused: string | null }) => void;
  invalidateWorktree: (slug: string) => Promise<void>;
  refreshOrigin: () => Promise<void>;
  refreshGithub: () => Promise<void>;
  refreshStack: () => Promise<void>;
  refreshTmuxSessions: () => Promise<void>;
  refreshRemoteWorktrees: () => Promise<unknown>;
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
 * Session-target messages report `launched: true` at hand-off (delivery
 * is fire-and-forget by design).
 */
export type LaunchOutcome = { launched: boolean; reason?: string };

const dispatchLog = createLogger("[actions]");
const io = operationErrors("useActionDispatch");

export function useActionDispatch(opts: ActionDispatchOpts): {
  launchAction: (
    slug: string,
    def: ActionDef | null,
    extras: string,
    arg?: string,
    launchOpts?: LaunchActionOpts,
    target?: WorktreeTarget,
  ) => Promise<LaunchOutcome>;
  launchSlotCommand: (
    slotSlug: string,
    def: ActionDef | null,
    extras: string,
  ) => Promise<LaunchOutcome>;
} {
  const queryClient = useQueryClient();
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
    refreshTmuxSessions: opts.refreshTmuxSessions,
    refreshRemoteWorktrees: opts.refreshRemoteWorktrees,
  });
  helpersRef.current = {
    invalidateWorktree: opts.invalidateWorktree,
    refreshOrigin: opts.refreshOrigin,
    refreshGithub: opts.refreshGithub,
    refreshStack: opts.refreshStack,
    refreshTmuxSessions: opts.refreshTmuxSessions,
    refreshRemoteWorktrees: opts.refreshRemoteWorktrees,
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
      const forkRefresh = (label: string, operation: () => Promise<unknown>): void => {
        forkReported(io.promise(label, operation), (error) =>
          dispatchLog.warn(`refresh failed: ${error.message}`),
        );
      };
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
          refreshTmuxSessions: rt,
          refreshRemoteWorktrees: rr,
        } = helpersRef.current;
        const remote = run.worktreeRef?.kind === "remote";
        for (const tag of run.affects) {
          switch (tag) {
            case "git":
              if (remote) forkRefresh("refresh remote worktrees", rr);
              else {
                forkRefresh("refresh origin", ro);
                forkRefresh(`invalidate ${run.slug}`, () => inv(run.slug));
              }
              // History-rewriting actions (rebase, modify, …) rewrite
              // commits under a fixed explicit parent, so the per-base
              // diff / sync queries need a re-run even though the parent
              // relationship is unchanged. `refreshStack` invalidates
              // those (see its doc in state/hooks.ts).
              if (!remote) forkRefresh("refresh stack", rs);
              break;
            case "github":
              forkRefresh("refresh github", rg);
              break;
            case "issue":
              forkRefresh("refresh issue statuses", () => queryClient.invalidateQueries({ queryKey: ["issueStatuses"] }));
              break;
            case "dev":
              // Dev-server start/stop — refresh the slug's per-worktree
              // fields so the dev row/bolt snap without waiting out the
              // staleTime. Slug-scoped; no cross-worktree state moved.
              if (remote) forkRefresh("refresh remote worktrees", rr);
              else {
                // The dev query key includes the batched tmux answer. Refresh
                // that source as well as the slug, or an immediate slug
                // refetch keeps using `sessionExists: false` and renders the
                // just-started server as stopped until the 5s backstop poll.
                forkRefresh("refresh tmux sessions", rt);
                forkRefresh(`invalidate ${run.slug}`, () => inv(run.slug));
              }
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
              ALL_BUILTIN_ACTIONS.find((d) => d.id === run.actionId) ??
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
    target?: WorktreeTarget,
  ): Promise<LaunchOutcome> {
    const { rows, actionSubjectFor, primaryHarness, toast, setFocus } = opts;
    // Automation launches (marked by `autoFireKeys`) suppress the
    // keystroke-style acks below: the engine already toasts its own
    // "auto <rule>: … — running <run>" dispatch line and narrates
    // declines/failures itself, so the direct toasts here would
    // clobber it in the single latest-wins slot (see the toast
    // contract in AGENTS.md — background paths toast via the logger).
    const manual = launchOpts.autoFireKeys === undefined;
    const ack: typeof toast = (...args) => {
      if (manual) toast(...args);
    };
    const localRow = rows.find((r) => r.wt.slug === slug);
    const subjectTarget = target ?? (localRow ? localWorktreeTarget(localRow.wt) : undefined);
    const subject = subjectTarget ? actionSubjectFor(subjectTarget) : undefined;
    if (!subject) {
      ack("worktree gone", theme.warn, 1500);
      return { launched: false, reason: "worktree gone" };
    }
    const remoteTarget = isRemoteWorktreeTarget(subject.target)
      ? subject.target
      : undefined;
    const runtimeKey = subject.actionKey;
    if (!def && !extras.trim()) {
      ack("prompt is empty", theme.warn, 1500);
      return { launched: false, reason: "prompt is empty" };
    }
    // Manager-target actions never touch the source worktree — they
    // send to the manager session and only READ the row for
    // template vars — so the destroy/busy gates below don't apply
    // (a needs-human fire happens exactly while the source session is
    // busy asking; gating on it would decline/retry forever).
    const isManagerTarget = def?.kind === "claude" && def.target === "manager";
    if (!isManagerTarget) {
      // Refuse if the worktree is being cleaned up (archived the instant a
      // destroy/clean dispatches, before the flock exists) or mid-destroy /
      // mid-init (flock held). The archived half matters: a clean/destroy
      // flips `row.archived` synchronously but the detached `_destroy`
      // child only grabs the flock a process-spawn later, so `lockStatus`
      // alone leaves a window where an action would launch into a directory
      // about to be `git worktree remove --force`d. `launchBlockedReason`
      // checks both — the same gate every session launch uses.
      const blocked = actionSubjectBlockedReason(subject);
      if (blocked) {
        ack(`${slug} is ${blocked}`, theme.warn, 2000);
        return { launched: false, reason: `${slug} is ${blocked}` };
      }
      if (subject.status.kind === StatusKind.Busy) {
        ack(`${slug} is busy`, theme.warn, 2000);
        return { launched: false, reason: `${slug} is busy` };
      }
    }
    if (def) {
      const avail = evaluateActionRequirements(def.requires, {
        slug,
        issueId: subject.issueId,
        pr: subject.pr,
        deployed: subject.deployed,
      });
      if (!avail.ok) {
        ack(`${def.name}: ${avail.reason}`, theme.warn, 2500);
        return { launched: false, reason: avail.reason };
      }
    }
    const prefix = actionSkillPrefix(def, primaryHarness);
    const baseVars = actionSubjectVars(subject, prefix);
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
      pendingArgs.current.set(`${runtimeKey}/${def.id}`, arg);
    }
    // Session- and manager-target prompt actions bypass the headless
    // `-p` runner and deliver the prompt to a live harness session
    // (starting its tmux host detached if needed): the worktree's
    // primary F12 session for `session`, the singleton manager session — prefixed
    // `[re: <slug>]` so the subject is explicit — for `manager`.
    // Fire-and-forget: there's no run to track or focus, so we just log
    // progress to the activity pane. The cold-start path can take a few
    // seconds, hence the immediate "sending…" toast. One shared
    // delivery helper so the two targets can't drift.
    if (def && def.kind === "claude" && (def.target === "session" || def.target === "manager")) {
      const renderedPrompt = applyVars(def.prompt, vars);
      const trimmedExtras = applyVars(extras, vars).trim();
      const body = trimmedExtras
        ? `${renderedPrompt}\n\n${trimmedExtras}`
        : renderedPrompt;
      const sessionLog = createLogger(
        remoteTarget ? `[remote:${remoteTarget.location.endpoint.label}]` : slug,
      );
      if (def.target === "session") {
        const label = "active agent session";
        const location = remoteTarget
          ? ` on ${remoteTarget.location.endpoint.label}`
          : "";
        sessionLog.event.info(
          `${def.name} → ${label}${location}`,
        );
        ack(`sending ${def.name} to ${label}${location}…`, theme.info, 2000);
        Effect.runFork(sendWorktreeMessage(
          subject.target,
          body,
          (line) => sessionLog.event.info(line),
        ).pipe(Effect.match({
          onFailure: (err) => {
            sessionLog.event.err(`send failed: ${err.message}`, { toast: true });
          },
          onSuccess: (res) => {
            if (res.ok && res.delivered === false) {
              sessionLog.attention.warn(
                `${label} never received ${def.name} — attach and check its pane`,
              );
            } else if (res.ok) {
              sessionLog.event.ok(
                `${res.coldStarted ? `started ${label} and sent` : `sent`} ${def.name}${
                  res.delivered === null ? " (arrival can't be confirmed)" : ""
                }`,
                { toast: true },
              );
            } else {
              sessionLog.event.err(`send failed: ${res.reason}`, { toast: true });
            }
          },
        })));
        return { launched: true };
      }

      const deliveryTarget = {
        slug: MANAGER_SLOT.slug,
        label: "manager",
        text: `[re: ${slug}] ${body}`,
      };
      sessionLog.event.info(`${def.name} → ${deliveryTarget.label}`);
      ack(`sending ${def.name} to ${deliveryTarget.label}…`, theme.info, 2000);
      Effect.runFork(sendAgentMessage(deliveryTarget.slug, deliveryTarget.text).pipe(Effect.match({
        onFailure: (err) => {
          sessionLog.event.err(`send failed: ${err.message}`, { toast: true });
        },
        onSuccess: (res) => {
          if (res.ok && res.delivered === false) {
            // Delivery is verified against the target's own transcript,
            // for EVERY harness including Claude — a dispatch that
            // can't be verified needs attention because an automation
            // has no other witness.
            sessionLog.attention.warn(
              `${deliveryTarget.label} never received ${def.name} — attach and check its pane`,
            );
          } else if (res.ok) {
            // Toast: the "sending…" ack above has long expired by the
            // time a cold start finishes, and this may be an automation
            // dispatch with no keystroke to acknowledge.
            // `delivered: null` is UNCONFIRMABLE, not confirmed: a
            // slash command is recorded as an expanded command entry,
            // so there is no prompt text to match. Say so rather than
            // claiming a verified send — but on the event feed, not the
            // attention feed: it's the expected outcome for a command,
            // and interrupting a scan for it every time would be noise.
            sessionLog.event.ok(
              `${res.coldStarted ? `started ${deliveryTarget.label} and sent` : `sent`} ${def.name}${
                res.coldStarted ? "" : ` to ${deliveryTarget.label}`
              }${res.delivered === null ? " (a command's arrival can't be confirmed)" : ""}`,
              { toast: true },
            );
          } else {
            // Logger-toast (not ctx.toast): the failure lands after the
            // dispatch ack expired, and it must flash for automation
            // launches too — this is their only failure surface.
            sessionLog.event.err(`send failed: ${res.reason}`, { toast: true });
          }
        },
      })));
      return { launched: true };
    }
    const result = def
      ? await Effect.runPromise(actionRegistry.startForWorktree(
          def,
          subject.target,
          config.paths.mainClone,
          extras,
          vars,
          primaryHarness,
          { autoFireKeys: launchOpts.autoFireKeys },
        ))
      : await Effect.runPromise(actionRegistry.startCustomForWorktree(
          subject.target,
          config.paths.mainClone,
          extras,
          vars,
          primaryHarness,
        ));
    if (!result.ok) {
      ack(`action: ${result.reason}`, theme.err, 3000);
      return { launched: false, reason: result.reason };
    }
    if (def?.kind === "shell" && def.issueStatus && vars.issue_id && config.issueTracker?.statusCommand) {
      forkReported(trackIssueStatusAction(queryClient, result.run, vars.issue_id, def.issueStatus),
        () => dispatchLog.warn("issue status update failed"));
    }
    // Clear this worktree's focus so the auto-rules surface the
    // just-launched action.
    setFocus(runtimeKey, { focused: null });
    ack(`launched ${result.run.actionName}`, theme.info, 2000);
    return { launched: true };
  }

  /**
   * Slot-scoped delivery — the launch path for the palettes' `fleet:
   * true` builtins and free-text messages, addressed to a session slot
   * (the `M` manager palette and the `<` / `>` / `\` slot palettes).
   * Unlike the row path above there is no subject worktree: no row
   * gates and no `[re: <slug>]` prefix. The builtin's own prompt still
   * renders through `applyVars` for the row-less vars (`{{today}}`);
   * the user's free text deliberately does NOT, since rendering it
   * would eat literal `{{…}}` they typed. Same fire-and-forget delivery
   * + logging contract otherwise.
   */
  async function launchSlotCommand(
    slotSlug: string,
    def: ActionDef | null,
    extras: string,
  ): Promise<LaunchOutcome> {
    const { toast } = opts;
    const slot = SESSION_SLOTS.find((s) => s.slug === slotSlug);
    if (!slot) {
      toast(`unknown slot ${slotSlug}`, theme.warn, 2000);
      return { launched: false, reason: `unknown slot ${slotSlug}` };
    }
    const trimmedExtras = extras.trim();
    const prompt = def && def.kind === "claude" ? applyVars(def.prompt, {}) : "";
    const body = trimmedExtras
      ? prompt
        ? `${prompt}\n\n${trimmedExtras}`
        : trimmedExtras
      : prompt;
    if (!body) {
      toast("prompt is empty", theme.warn, 1500);
      return { launched: false, reason: "prompt is empty" };
    }
    const label = def?.name ?? "custom message";
    const slotLog = createLogger(slot.slug);
    slotLog.event.info(`${label} → ${slot.label}`);
    toast(`sending ${label} to ${slot.label}…`, theme.info, 2000);
    const send = def?.id === "manager-compact" || def?.id === "slot-compact"
      ? sendAgentCompact(slot.slug, body)
      : sendAgentMessage(slot.slug, body);
    Effect.runFork(send.pipe(Effect.match({
      onFailure: (err) => {
        slotLog.event.err(`send failed: ${err.message}`, { toast: true });
      },
      onSuccess: (res) => {
        if ("preparation" in res && (res as CompactResult).preparation) {
          if (res.ok && res.delivered !== false) slotLog.event.info(
            `preparation received; submitted /compact to ${slot.label} (execution unconfirmed)`, { toast: true },
          );
          else slotLog.event.err(`send failed: ${res.ok ? "command not delivered" : res.reason}`, { toast: true });
          return;
        }
        if (res.ok && res.delivered === false) {
          // See the row path above: an unverified briefing stays visible.
          slotLog.attention.warn(
            `${slot.label} never received ${label} — attach and check its pane`,
          );
        } else if (res.ok) {
          // Toast: the "sending…" ack above has long expired by the time
          // a cold start finishes.
          // `delivered: null` is unconfirmable, not confirmed — the
          // manager palette's `M m` (`/compact`) is exactly this case.
          slotLog.event.ok(
            `${
              res.coldStarted
                ? `started ${slot.label} and sent ${label}`
                : `sent ${label} to ${slot.label}`
            }${res.delivered === null ? " (a command's arrival can't be confirmed)" : ""}`,
            { toast: true },
          );
        } else {
          slotLog.event.err(`send failed: ${res.reason}`, { toast: true });
        }
      },
    })));
    return { launched: true };
  }

  return { launchAction, launchSlotCommand };
}
