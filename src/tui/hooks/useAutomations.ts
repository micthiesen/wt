/**
 * The `[[automations]]` engine: watches the aggregated row state,
 * evaluates trigger conditions (`tui/automation-rules.ts`), dedupes
 * against the persistent fire ledger (`core/automations.ts`), holds
 * intents until the target settles, and dispatches through the same
 * paths a keystroke would (`launchAction`, the clean flow, the
 * algorithmic restack).
 *
 * # The queue holds intents, not commands
 *
 * A queued entry is `(rule, target)` — a set, keyed by
 * `fireIdentity`. Everything about it is re-derived at delivery time:
 * an intent whose condition cleared while queued is dropped
 * ("superseded"), one whose head SHA moved fires under the new key,
 * and bursts of state churn collapse into the one live intent. Because
 * conditions are level-based, the queue is pure derived state — it's
 * deliberately NOT persisted; a restart rebuilds it from conditions
 * that still hold minus fire keys already in the ledger.
 *
 * # Dispatch ordering (the one hard rule)
 *
 * `markFiresDispatched` runs SYNCHRONOUSLY before any await in the
 * dispatch path, so a concurrent pass can never double-fire. Delivery
 * flips the keys to `delivered` when the launch resolves; a failed
 * launch is ALSO marked delivered — automations never retry on their
 * own (a new push = a new fire key = the sanctioned retry).
 *
 * # Loop protection, layered
 *
 * fire keys (once per failure instance) → delivery-time re-validation
 * (stale remedies never run) → settle window (quiescence + human
 * cancellation grace) → per-rule cooldown → circuit breaker (two
 * consecutive dispatches without the condition ever clearing trips the
 * rule for that worktree until someone actually fixes it).
 */
import { useEffect, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";

import {
  actionRegistry,
  BUILTIN_ACTIONS,
  evaluateActionRequirements,
  type ActionDef,
} from "../../core/actions.ts";
import {
  BREAKER_LIMIT,
  breakerState,
  bumpBreaker,
  hasHandledFire,
  lastDispatchAt,
  lastWorktreeEditAt,
  markFiresDelivered,
  markFiresDispatched,
  reconcileDispatchedFires,
  resetBreaker,
  tripBreaker,
} from "../../core/automations.ts";
import { config } from "../../core/config.ts";
import { lockStatus } from "../../core/locks.ts";
import { createLogger } from "../../core/logger.ts";
import { StatusKind } from "../../core/types.ts";
import { useGithub } from "../../state/hooks.ts";
import { wtStateQuery } from "../../state/queries.ts";

import {
  evaluateAutomations,
  fireIdentity,
  type AutomationFire,
} from "../automation-rules.ts";
import { isCleanCandidate } from "../app-helpers.ts";
import type { ActiveSessionGlyph } from "./useHarnessSessions.ts";
import type { LaunchActionOpts } from "./useActionDispatch.ts";
import type { WorktreeRow } from "./useWorktreeRows.ts";

const log = createLogger("[auto]");

/** Live fetch marker: github data older than this process isn't trusted. */
const APP_START = Date.now();

/** Cap on concurrently-executing auto dispatches across all worktrees. */
const MAX_CONCURRENT = 2;

/** Re-evaluation heartbeat — ages queued intents past their settle window. */
const TICK_MS = 15_000;

/** Evaluation debounce over row churn. */
const PASS_DEBOUNCE_MS = 500;

/** Session-target dispatches release their concurrency slot after this. */
const SESSION_SLOT_MAX_MS = 10 * 60 * 1000;

type Intent = {
  id: string;
  fire: AutomationFire;
  createdAt: number;
  announced: boolean;
};

type Executing = {
  slug: string;
  kind: "builtin" | "headless" | "session";
  promiseDone: boolean;
  dispatchedAt: number;
};

export type AutomationsOpts = {
  rows: readonly WorktreeRow[];
  activeSessionBySlug: ReadonlyMap<string, ActiveSessionGlyph>;
  launchAction: (
    slug: string,
    def: ActionDef | null,
    extras: string,
    arg?: string,
    launchOpts?: LaunchActionOpts,
  ) => Promise<void>;
  doCleanSlugs: (slugs: readonly string[]) => Promise<void>;
  doRestackStack: (stackId: string) => Promise<boolean>;
};

export type AutomationsState = {
  /** True when the config defines any [[automations]] rules. */
  configured: boolean;
  /** Global session pause (the `A` keybind). */
  paused: boolean;
  togglePaused: () => boolean;
  /** Queued (not yet dispatched) intents, for the title-bar indicator. */
  pendingCount: number;
};

/** The breaker/cooldown identity: stacks key on stackId (the target
 *  slug — "first open slice" — shifts as slices land). */
function pairTarget(fire: AutomationFire): string {
  return fire.stackId ?? fire.slug;
}

function resolveActionDef(runId: string): ActionDef | null {
  return (
    config.actions.find((d) => d.id === runId) ??
    BUILTIN_ACTIONS.find((d) => d.id === runId) ??
    null
  );
}

export function useAutomations(opts: AutomationsOpts): AutomationsState {
  const rules = config.automations;
  const configured = rules.length > 0;
  const [paused, setPaused] = useState(false);
  const [pendingCount, setPendingCount] = useState(0);

  const github = useGithub();
  const wtState = useQuery(wtStateQuery());
  const githubFresh = (github.dataUpdatedAt ?? 0) > APP_START;

  // Everything the pass reads lives in a ref so the effects subscribe
  // once and never tear down mid-flight (same pattern as
  // `useActionDispatch`'s registry subscriber).
  const latest = useRef({
    rows: opts.rows,
    sessions: opts.activeSessionBySlug,
    launchAction: opts.launchAction,
    doCleanSlugs: opts.doCleanSlugs,
    doRestackStack: opts.doRestackStack,
    githubFresh,
    pausedSlugs: new Set<string>(),
    paused,
  });
  const pausedSlugs = new Set<string>();
  for (const [slug, st] of Object.entries(wtState.data?.slugs ?? {})) {
    if (st.automationsPaused === true) pausedSlugs.add(slug);
  }
  latest.current = {
    rows: opts.rows,
    sessions: opts.activeSessionBySlug,
    launchAction: opts.launchAction,
    doCleanSlugs: opts.doCleanSlugs,
    doRestackStack: opts.doRestackStack,
    githubFresh,
    pausedSlugs,
    paused,
  };

  const intents = useRef<Map<string, Intent>>(new Map());
  const executing = useRef<Map<string, Executing>>(new Map());
  const passTimer = useRef<Timer | null>(null);

  // Boot reconciliation: match ledger entries stuck in `dispatched`
  // against the fire keys stamped into rehydrated action runs
  // (actionRegistry.boot ran before first render). Matched → the run
  // really launched, flip to delivered; unmatched → the dispatch died
  // mid-window, drop the entry so the still-true condition re-fires.
  useEffect(() => {
    if (!configured) return;
    const runKeys = new Set<string>();
    for (const run of actionRegistry.getSnapshot().values()) {
      for (const k of run.autoFireKeys ?? []) runKeys.add(k);
    }
    const dropped = reconcileDispatchedFires((k) => runKeys.has(k));
    if (dropped > 0) {
      log.info("boot: dropped interrupted dispatches", { dropped });
    }
  }, [configured]);

  function sessionBusyState(slug: string): string | null {
    const sess = latest.current.sessions.get(slug);
    if (!sess) return null;
    // ActiveSessionGlyph only exists for LIVE sessions. waiting/idle/
    // abandoned are safe to inject into; working/asking/polling are
    // not, and a live session with no derived state (codex/opencode
    // without event data) is treated as busy — conservative, because
    // the paste's trailing Enter could answer a dialog we can't see.
    if (sess.state === "waiting" || sess.state === "idle" || sess.state === "abandoned") {
      return null;
    }
    return sess.state ?? "live";
  }

  /** Null when every quiesce slug is deliverable; else a human reason. */
  function quiesceBlockReason(fire: AutomationFire, now: number): string | null {
    const settleMs = fire.rule.settleSeconds * 1000;
    for (const slug of fire.quiesceSlugs) {
      const row = latest.current.rows.find((r) => r.wt.slug === slug);
      // A quiesce member that vanished (cleaned mid-queue) doesn't block.
      if (!row) continue;
      if (row.status.kind === StatusKind.Busy) return `${slug} is busy`;
      if (lockStatus(slug)) return `${slug} is locked`;
      if (actionRegistry.get(slug)?.status === "running") {
        return `action running on ${slug}`;
      }
      const sessState = sessionBusyState(slug);
      if (sessState) return `session ${sessState} on ${slug}`;
      if (now - lastWorktreeEditAt(slug) < settleMs) {
        return `recent edits in ${slug}`;
      }
    }
    return null;
  }

  /** True once an executing entry no longer occupies a concurrency slot. */
  function isReleased(ex: Executing, now: number): boolean {
    if (!ex.promiseDone) return false;
    switch (ex.kind) {
      case "builtin":
        return true;
      case "headless":
        return actionRegistry.get(ex.slug)?.status !== "running";
      case "session":
        // The injected prompt runs invisibly inside the session; hold
        // the slot while it's observably working, with a hard cap so a
        // long manual session can't pin the slot forever.
        return (
          sessionBusyState(ex.slug) === null ||
          now - ex.dispatchedAt > SESSION_SLOT_MAX_MS
        );
      default: {
        const _exhaustive: never = ex.kind;
        void _exhaustive;
        return true;
      }
    }
  }

  async function execute(fire: AutomationFire): Promise<void> {
    const { rule, slug, stackId } = fire;
    const wtLog = createLogger(slug);
    if (rule.run === "builtin:restack") {
      if (!stackId) throw new Error("builtin:restack fire without a stackId");
      // Pre-clean the landed slices (recomputed against CURRENT rows,
      // not the rows the fire was born under), then reconcile + replay.
      // doCleanSlugs re-filters through isCleanCandidate, so a slice
      // that un-merged in between can't be destroyed.
      const mergedSlugs = latest.current.rows
        .filter(
          (r) =>
            r.stack?.stackId === stackId &&
            !r.stack.isHolistic &&
            isCleanCandidate(r),
        )
        .map((r) => r.wt.slug);
      if (mergedSlugs.length > 0) {
        wtLog.event.info(
          `auto ${rule.id}: cleaning merged slice${mergedSlugs.length === 1 ? "" : "s"} ${mergedSlugs.join(", ")}`,
        );
        await latest.current.doCleanSlugs(mergedSlugs);
      }
      await latest.current.doRestackStack(stackId);
      return;
    }
    if (rule.run === "builtin:clean") {
      await latest.current.doCleanSlugs([slug]);
      return;
    }
    const def = resolveActionDef(rule.run);
    if (!def) throw new Error(`action "${rule.run}" not found in config`);
    await latest.current.launchAction(slug, def, "", undefined, {
      autoFireKeys: fire.fireKeys,
    });
  }

  function dispatchKind(rule: AutomationFire["rule"]): Executing["kind"] {
    if (rule.run.startsWith("builtin:")) return "builtin";
    const def = resolveActionDef(rule.run);
    if (def?.kind === "claude" && def.target === "session") return "session";
    return "headless";
  }

  function runPass(): void {
    if (!configured) return;
    const now = Date.now();
    const ctx = latest.current;

    // Release finished dispatches so their concurrency slots free up.
    for (const [id, ex] of executing.current) {
      if (isReleased(ex, now)) executing.current.delete(id);
    }

    if (ctx.paused) {
      if (intents.current.size > 0) {
        log.event.dim(
          `automations paused — dropped ${intents.current.size} pending intent${intents.current.size === 1 ? "" : "s"}`,
        );
        intents.current.clear();
      }
      setPendingCount(0);
      return;
    }

    const fires = evaluateAutomations(rules, ctx.rows, {
      githubFresh: ctx.githubFresh,
      isPausedSlug: (slug) => ctx.pausedSlugs.has(slug),
    });
    const byId = new Map(fires.map((f) => [fireIdentity(f), f] as const));

    // Breaker resets: a (rule, target) with breaker state whose
    // condition is now observed FALSE means the failure actually
    // cleared — the consecutive count starts over. Gated on fresh
    // github data so a boot-stale pass can't reset a real trip.
    if (ctx.githubFresh) {
      for (const rule of rules) {
        if (rule.on === "stack.parent_merged") {
          for (const row of ctx.rows) {
            const sid = row.stack?.stackId;
            if (sid && !byId.has(`${rule.id}|${sid}`)) resetBreaker(rule.id, sid);
          }
        } else {
          for (const row of ctx.rows) {
            if (!byId.has(`${rule.id}|${row.wt.slug}`)) {
              resetBreaker(rule.id, row.wt.slug);
            }
          }
        }
      }
    }

    // Upsert intents for fires with at least one unseen key; refresh
    // the fire payload on existing intents so delivery always uses
    // current keys/details.
    for (const [id, fire] of byId) {
      if (executing.current.has(id)) continue;
      const unseen = fire.fireKeys.some((k) => !hasHandledFire(k));
      if (!unseen) {
        intents.current.delete(id);
        continue;
      }
      const existing = intents.current.get(id);
      if (existing) existing.fire = fire;
      else intents.current.set(id, { id, fire, createdAt: now, announced: false });
    }

    // Drop superseded intents — the condition cleared while queued.
    for (const [id, intent] of intents.current) {
      if (!byId.has(id)) {
        if (intent.announced) {
          createLogger(intent.fire.slug).event.dim(
            `auto ${intent.fire.rule.id}: superseded (condition cleared) — dropped`,
          );
        }
        intents.current.delete(id);
      }
    }

    // Delivery, FIFO by intent age, bounded by the concurrency cap.
    const queue = [...intents.current.values()].sort(
      (a, b) => a.createdAt - b.createdAt,
    );
    for (const intent of queue) {
      if (executing.current.size >= MAX_CONCURRENT) break;
      const { fire } = intent;
      const { rule } = fire;
      const wtLog = createLogger(fire.slug);
      if (!intent.announced) {
        intent.announced = true;
        wtLog.event.info(`auto ${rule.id}: ${fire.detail} — queued`);
      }
      const target = pairTarget(fire);
      const breaker = breakerState(rule.id, target);
      if (breaker.trippedAt !== null) {
        // Breaker is open: swallow the fire (mark handled) so it
        // doesn't re-announce every pass. Resets when the condition
        // is observed clear.
        markFiresDelivered(fire.fireKeys);
        wtLog.event.dim(`auto ${rule.id}: breaker open — skipping`);
        intents.current.delete(intent.id);
        continue;
      }
      if (rule.cooldownMinutes !== null) {
        const last = lastDispatchAt(rule.id, target);
        if (last !== null && now - last < rule.cooldownMinutes * 60_000) continue;
      }
      // Settle window: minimum intent age (the cancellation grace
      // period); the per-slug edit-recency half lives in
      // quiesceBlockReason.
      if (now - intent.createdAt < rule.settleSeconds * 1000) continue;
      const blocked = quiesceBlockReason(fire, now);
      if (blocked) {
        if (rule.busy === "skip") {
          markFiresDelivered(fire.fireKeys);
          wtLog.event.dim(`auto ${rule.id}: skipped (${blocked})`);
          intents.current.delete(intent.id);
        }
        continue;
      }
      // Action preconditions (requires tags) — unmet keeps the intent
      // pending; row state may still change (e.g. a draft flips ready).
      const def = rule.run.startsWith("builtin:") ? null : resolveActionDef(rule.run);
      if (def) {
        const row = ctx.rows.find((r) => r.wt.slug === fire.slug);
        const avail = evaluateActionRequirements(def.requires, {
          pr: row?.pr,
          deployed: row?.fields.deploy.data ?? false,
        });
        if (!avail.ok) continue;
      }
      if (breaker.count >= BREAKER_LIMIT) {
        tripBreaker(rule.id, target);
        markFiresDelivered(fire.fireKeys);
        wtLog.event.err(
          `auto ${rule.id} tripped breaker on ${target} — ${BREAKER_LIMIT} runs, condition never cleared; fix by hand to re-arm`,
        );
        intents.current.delete(intent.id);
        continue;
      }

      // Dispatch. Ledger write is synchronous BEFORE the async launch —
      // the once-only guarantee lives on this line.
      markFiresDispatched(fire.fireKeys, rule.id, target);
      bumpBreaker(rule.id, target);
      intents.current.delete(intent.id);
      const entry: Executing = {
        slug: fire.slug,
        kind: dispatchKind(rule),
        promiseDone: false,
        dispatchedAt: now,
      };
      executing.current.set(intent.id, entry);
      wtLog.event.info(`auto ${rule.id}: ${fire.detail} — running ${rule.run}`);
      void execute(fire)
        .then(
          () => {
            markFiresDelivered(fire.fireKeys);
          },
          (err) => {
            // Failure does NOT retry: keys stay handled; a new push
            // (new fire key) is the sanctioned retry path.
            markFiresDelivered(fire.fireKeys);
            const msg = err instanceof Error ? err.message : String(err);
            wtLog.event.err(`auto ${rule.id} failed: ${msg}`);
          },
        )
        .finally(() => {
          entry.promiseDone = true;
          schedulePass();
        });
    }

    setPendingCount(intents.current.size);
  }

  function schedulePass(): void {
    if (passTimer.current) return;
    passTimer.current = setTimeout(() => {
      passTimer.current = null;
      runPass();
    }, PASS_DEBOUNCE_MS);
  }

  // Re-evaluate whenever the observable inputs change…
  useEffect(() => {
    if (!configured) return;
    schedulePass();
  });

  // …and on a heartbeat, so queued intents age past their settle
  // window / cooldowns without needing external churn.
  useEffect(() => {
    if (!configured) return;
    const t = setInterval(() => runPass(), TICK_MS);
    return () => {
      clearInterval(t);
      if (passTimer.current) {
        clearTimeout(passTimer.current);
        passTimer.current = null;
      }
    };
  }, [configured]);

  return {
    configured,
    paused,
    togglePaused: () => {
      const next = !paused;
      setPaused(next);
      log.event.info(next ? "automations paused" : "automations resumed");
      schedulePass();
      return next;
    },
    pendingCount,
  };
}
