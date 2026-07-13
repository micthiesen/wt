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
 *
 * Known asymmetry: push-scoped triggers (checks / rabbit / review /
 * conflict) get a natural retry on the next push (new SHA = new key),
 * but `wt.merged` / `stack.parent_merged` keys never change — a run
 * that launched and FAILED consumes them for good, and the activity
 * pane error line is the escalation (run `c` / `R` by hand). Declined
 * dispatches (contention with a manual launch) are different: those
 * un-consume the fire and retry once the contention clears.
 */
import { useEffect, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";

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
  dropFires,
  hasHandledFire,
  lastDispatchAt,
  lastWorktreeEditAt,
  markFiresDelivered,
  markFiresDispatched,
  reconcileDispatchedFires,
  resetBreaker,
  tripBreaker,
} from "../../core/automations.ts";
import { config, type AutomationTrigger } from "../../core/config.ts";
import { lockStatus } from "../../core/locks.ts";
import { createLogger } from "../../core/logger.ts";
import { StatusKind } from "../../core/types.ts";
import { toggleGlobalAutomationsPaused } from "../../core/wtstate.ts";
import { wtStateQuery } from "../../state/queries.ts";

import {
  evaluateAutomations,
  fireIdentity,
  type AutomationFire,
} from "../automation-rules.ts";
import { isCleanCandidate } from "../app-helpers.ts";
import type { ActiveSessionGlyph } from "./useHarnessSessions.ts";
import type { LaunchActionOpts, LaunchOutcome } from "./useActionDispatch.ts";
import type { WorktreeRow } from "./useWorktreeRows.ts";

const log = createLogger("[auto]");

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
  /** All slugs the dispatch may touch (quiesceSlugs at dispatch time). */
  slugs: readonly string[];
  kind: "builtin" | "headless" | "session";
  /** True for builtin:restack — at most one may execute PER STACK at a
   *  time (the engine locks per chain, so different stacks restack
   *  concurrently). */
  isRestack: boolean;
  /** The restack's stack id (fire.stackId) — the per-stack in-flight
   *  key. Null for non-restack dispatches. */
  stackId: string | null;
  promiseDone: boolean;
  dispatchedAt: number;
};

/** Outcome of one dispatch's async half. `declined` = a contention
 *  guard refused BEFORE anything ran; the fire gets un-consumed. */
type ExecuteOutcome = { declined: string | null };

/** Triggers whose condition needs fresh github data to evaluate at all.
 *  Their breaker resets are gated on freshness too — a boot-stale pass
 *  can't observe "condition cleared". `pr.conflict` is fully local, so
 *  its breaker must reset even when github never fetches this session
 *  (offline, no token), or a trip would wedge forever. */
const GITHUB_DRIVEN: ReadonlySet<AutomationTrigger> = new Set([
  "pr.checks.failed",
  "rabbit.unresolved",
  "review.changes_requested",
  "wt.merged",
  "stack.parent_merged",
]);

export type AutomationsOpts = {
  rows: readonly WorktreeRow[];
  activeSessionBySlug: ReadonlyMap<string, ActiveSessionGlyph>;
  launchAction: (
    slug: string,
    def: ActionDef | null,
    extras: string,
    arg?: string,
    launchOpts?: LaunchActionOpts,
  ) => Promise<LaunchOutcome>;
  doCleanSlugs: (slugs: readonly string[]) => Promise<void>;
  doRestackStack: (stackId: string) => Promise<"clean" | "failed" | "busy">;
  /** Peek at one stack's restack-in-flight state (manual `R` shares it). */
  isRestackBusy: (stackId: string) => boolean;
};

export type AutomationsState = {
  /** True when the config defines any [[automations]] rules. */
  configured: boolean;
  /** Global pause (Shift+A). Persisted in wtstate across restarts. */
  paused: boolean;
  togglePaused: () => Promise<boolean>;
  /** Queued (not yet dispatched) intents, for the title-bar indicator. */
  pendingCount: number;
};

/** The breaker/cooldown identity: stacks key on stackId (the target
 *  slug — "first open member" — shifts as members land; the id churns
 *  when the root itself lands, resetting that pair's state). */
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
  const [pendingCount, setPendingCount] = useState(0);

  const qc = useQueryClient();
  const wtState = useQuery(wtStateQuery());
  // Global pause lives in wtstate (persisted across restarts, toggled
  // via Shift+A). Until the state file loads, treat as paused — the
  // engine must not fire before it knows the pause flags.
  const wtStateReady = wtState.data !== undefined;
  const paused = !wtStateReady || wtState.data.automationsPaused === true;
  // "Fresh" = a FETCH-driven success on the github query this session.
  // Deliberately not `dataUpdatedAt > appStart`: optimistic patches
  // (`setQueriesData` in the mark-ready / auto-merge / reviewer flows)
  // bump `dataUpdatedAt` on the whole cached blob without any network
  // round-trip, which would forge freshness for every OTHER PR still
  // sitting on restored persisted data. The cache subscription filters
  // to non-manual successes — the same manual-flag discrimination the
  // clobber guard in `runOptimisticMutation` uses.
  const [githubFresh, setGithubFresh] = useState(false);
  useEffect(() => {
    if (!configured || githubFresh) return;
    const unsubscribe = qc.getQueryCache().subscribe((event) => {
      if (event.type !== "updated") return;
      if (event.action.type !== "success") return;
      if ((event.action as { manual?: boolean }).manual) return;
      if (event.query.queryKey[0] !== "github") return;
      setGithubFresh(true);
    });
    return unsubscribe;
  }, [configured, githubFresh, qc]);

  // Everything the pass reads lives in a ref so the effects subscribe
  // once and never tear down mid-flight (same pattern as
  // `useActionDispatch`'s registry subscriber).
  const latest = useRef({
    rows: opts.rows,
    sessions: opts.activeSessionBySlug,
    launchAction: opts.launchAction,
    doCleanSlugs: opts.doCleanSlugs,
    doRestackStack: opts.doRestackStack,
    isRestackBusy: opts.isRestackBusy,
    githubFresh,
    pausedSlugs: new Set<string>(),
    paused,
  });
  // Effective per-worktree pause set: individually-paused slugs plus
  // every member of a paused STACK (Ctrl+A on any stack row pauses by
  // stackId, so members stacked on later are covered too; the toggle
  // also mirrors per-slug flags so the pause survives a re-root).
  const pausedSlugs = new Set<string>();
  for (const [slug, st] of Object.entries(wtState.data?.slugs ?? {})) {
    if (st.automationsPaused === true) pausedSlugs.add(slug);
  }
  const pausedStackIds = new Set(wtState.data?.pausedStacks ?? []);
  if (pausedStackIds.size > 0) {
    for (const row of opts.rows) {
      if (row.stack && pausedStackIds.has(row.stack.stackId)) {
        pausedSlugs.add(row.wt.slug);
      }
    }
  }
  latest.current = {
    rows: opts.rows,
    sessions: opts.activeSessionBySlug,
    launchAction: opts.launchAction,
    doCleanSlugs: opts.doCleanSlugs,
    doRestackStack: opts.doRestackStack,
    isRestackBusy: opts.isRestackBusy,
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

  async function execute(fire: AutomationFire): Promise<ExecuteOutcome> {
    const { rule, slug, stackId } = fire;
    const wtLog = createLogger(slug);
    if (rule.run === "builtin:restack") {
      if (!stackId) throw new Error("builtin:restack fire without a stackId");
      // Busy check FIRST, before the pre-clean: a manual `R` running on
      // THIS stack means nothing ran, so decline (un-consume the fire)
      // while the trigger condition is still intact. After the
      // pre-clean the condition is consumed (merged members destroyed),
      // so a busy chain there is a loud FAILURE, not a decline —
      // re-deriving the fire is no longer possible. Restacks of other
      // stacks run concurrently and don't block this one.
      if (latest.current.isRestackBusy(stackId)) {
        return { declined: "restack already running on this stack" };
      }
      // Pre-clean the landed members (recomputed against CURRENT rows,
      // not the rows the fire was born under — doCleanSlugs re-filters
      // through isCleanCandidate, so a member that un-merged can't be
      // destroyed), then reconcile + replay. Landed members include a
      // merged EXTERNAL parent (stack-on-stack boundary): its own
      // stack's records get reparented by the clean flow, and this
      // stack's reconcile reparents onto trunk. Paused rows are never
      // touched.
      const memberRows = latest.current.rows.filter(
        (r) => r.stack?.stackId === stackId,
      );
      const memberBranches = new Set(memberRows.map((r) => r.wt.branch));
      const externalParentSlugs = new Set<string>();
      for (const m of memberRows) {
        const so = m.stackedOn;
        if (so?.slug && !memberBranches.has(so.branch)) {
          externalParentSlugs.add(so.slug);
        }
      }
      const mergedSlugs = latest.current.rows
        .filter(
          (r) =>
            (r.stack?.stackId === stackId ||
              externalParentSlugs.has(r.wt.slug)) &&
            !latest.current.pausedSlugs.has(r.wt.slug) &&
            isCleanCandidate(r),
        )
        .map((r) => r.wt.slug);
      if (mergedSlugs.length > 0) {
        wtLog.event.info(
          `auto ${rule.id}: cleaning merged member${mergedSlugs.length === 1 ? "" : "s"} ${mergedSlugs.join(", ")}`,
        );
        await latest.current.doCleanSlugs(mergedSlugs);
      }
      // Target the restack at a SURVIVING member's branch, never the
      // stack id: the id is the ROOT's branch, and when the merged
      // member is the root (the common bottom-up landing) the pre-clean
      // just destroyed that branch — it no longer resolves a chain.
      // `fire.slug` is the first open member, which the pre-clean never
      // touches; the engine resolves the whole surviving stack from it.
      const targetRow = latest.current.rows.find((r) => r.wt.slug === slug);
      const outcome = await latest.current.doRestackStack(
        targetRow?.wt.branch ?? stackId,
      );
      if (outcome === "busy") {
        // Lost the mutex in the window between the peek above and the
        // engine acquiring it (a manual `R` mid-dispatch). The merged
        // members are already cleaned, so surface it as a failure that
        // names the manual follow-up instead of silently retrying.
        throw new Error(
          "restack engine grabbed by another run after the pre-clean — press R (or /restack) once it's free",
        );
      }
      return { declined: null };
    }
    if (rule.run === "builtin:clean") {
      await latest.current.doCleanSlugs([slug]);
      return { declined: null };
    }
    const def = resolveActionDef(rule.run);
    if (!def) throw new Error(`action "${rule.run}" not found in config`);
    const outcome = await latest.current.launchAction(slug, def, "", undefined, {
      autoFireKeys: fire.fireKeys,
    });
    // A launch the guards refused (action already running, busy row,
    // unmet requirements at the last instant) never ran — un-consume.
    return {
      declined: outcome.launched ? null : (outcome.reason ?? "launch declined"),
    };
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
    // cleared — the consecutive count starts over. Github-driven
    // triggers gate the reset on freshness (a boot-stale pass can't
    // observe "cleared"); purely local ones (pr.conflict) reset
    // unconditionally, or an offline session could wedge a trip
    // forever.
    for (const rule of rules) {
      if (GITHUB_DRIVEN.has(rule.on) && !ctx.githubFresh) continue;
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

    // Upsert intents for fires with at least one unseen key; refresh
    // the fire payload on existing intents so delivery always uses
    // current keys/details. A fire whose KEY SET changed is a new
    // failure instance (a fresh push while the old one was queued) —
    // its settle clock and announcement restart so the grace period is
    // real for the thing that will actually be remediated.
    for (const [id, fire] of byId) {
      if (executing.current.has(id)) continue;
      const unseen = fire.fireKeys.some((k) => !hasHandledFire(k));
      if (!unseen) {
        intents.current.delete(id);
        continue;
      }
      const existing = intents.current.get(id);
      if (existing) {
        const sameKeys =
          existing.fire.fireKeys.length === fire.fireKeys.length &&
          existing.fire.fireKeys.every((k, i) => fire.fireKeys[i] === k);
        if (!sameKeys) {
          existing.createdAt = now;
          existing.announced = false;
        }
        existing.fire = fire;
      } else {
        intents.current.set(id, { id, fire, createdAt: now, announced: false });
      }
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
    // Two contention gates keep dispatches from racing each other into
    // the non-throwing guards downstream (which would consume fires
    // for work that never ran): no two in-flight dispatches may touch
    // the same slug, and at most one builtin:restack runs PER STACK at
    // a time (the engine locks per chain — shared with manual `R` —
    // so different stacks restack concurrently).
    const occupiedSlugs = new Set<string>();
    const restackStacksInFlight = new Set<string>();
    for (const ex of executing.current.values()) {
      for (const s of ex.slugs) occupiedSlugs.add(s);
      if (ex.isRestack && ex.stackId) restackStacksInFlight.add(ex.stackId);
    }
    const queue = [...intents.current.values()].sort(
      (a, b) => a.createdAt - b.createdAt,
    );
    for (const intent of queue) {
      if (executing.current.size >= MAX_CONCURRENT) break;
      const { fire } = intent;
      const { rule } = fire;
      const wtLog = createLogger(fire.slug);
      const isRestack = rule.run === "builtin:restack";
      if (fire.quiesceSlugs.some((s) => occupiedSlugs.has(s))) continue;
      if (
        isRestack &&
        fire.stackId !== null &&
        (restackStacksInFlight.has(fire.stackId) ||
          ctx.isRestackBusy(fire.stackId))
      ) {
        continue;
      }
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
      // the once-only guarantee lives on this line. The breaker bump
      // waits for the settle handler: a dispatch a downstream guard
      // DECLINES (contention with a manual launch in the sub-second
      // window after the gates above) never ran, so it must count
      // toward neither the ledger nor the breaker.
      markFiresDispatched(fire.fireKeys, rule.id, target);
      intents.current.delete(intent.id);
      const entry: Executing = {
        slug: fire.slug,
        slugs: fire.quiesceSlugs,
        kind: dispatchKind(rule),
        isRestack,
        stackId: isRestack ? fire.stackId : null,
        promiseDone: false,
        dispatchedAt: now,
      };
      executing.current.set(intent.id, entry);
      for (const s of fire.quiesceSlugs) occupiedSlugs.add(s);
      if (isRestack && fire.stackId) restackStacksInFlight.add(fire.stackId);
      wtLog.event.info(`auto ${rule.id}: ${fire.detail} — running ${rule.run}`);
      void execute(fire)
        .then(
          (outcome) => {
            if (outcome.declined) {
              // Un-consume: the fire never ran. The still-true
              // condition re-derives an intent (with a fresh settle
              // window) on a later pass.
              dropFires(fire.fireKeys);
              executing.current.delete(intent.id);
              wtLog.event.dim(
                `auto ${rule.id}: declined (${outcome.declined}) — will retry once clear`,
              );
              return;
            }
            markFiresDelivered(fire.fireKeys);
            bumpBreaker(rule.id, target);
          },
          (err) => {
            // A run that LAUNCHED and failed does NOT retry: keys stay
            // handled; a new push (new fire key) is the sanctioned
            // retry path.
            markFiresDelivered(fire.fireKeys);
            bumpBreaker(rule.id, target);
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

  // Re-evaluate whenever the observable inputs change. `rows` and the
  // session map are identity-stabilized upstream (useWorktreeRows'
  // rowCache, useActiveSessionsBySlug's memo), so this fires on real
  // state churn, not render noise; the heartbeat below covers pure
  // time-based aging (settle windows, cooldowns).
  useEffect(() => {
    if (!configured) return;
    schedulePass();
  }, [
    configured,
    opts.rows,
    opts.activeSessionBySlug,
    wtState.data,
    githubFresh,
    paused,
  ]);

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
    togglePaused: async () => {
      const next = toggleGlobalAutomationsPaused();
      log.event.info(next ? "automations paused" : "automations resumed");
      await qc.invalidateQueries({ queryKey: wtStateQuery().queryKey });
      schedulePass();
      return next;
    },
    pendingCount,
  };
}
