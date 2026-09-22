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
 * Known asymmetry: push-scoped triggers (checks / review bot / review /
 * conflict) get a natural retry on the next push (new SHA = new key),
 * but `wt.merged` / `stack.parent_merged` keys never change — a run
 * that launched and FAILED consumes them for good, and the activity
 * pane error line is the escalation (run `c` / `R` by hand). Declined
 * dispatches (contention with a manual launch) are different: those
 * un-consume the fire and retry once the contention clears.
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Effect, Fiber } from "effect";

import {
  actionRegistry,
  ALL_BUILTIN_ACTIONS,
  evaluateActionRequirements,
  type ActionDef,
} from "../../core/actions.ts";
import type { AutomationDef } from "../../core/config.ts";
import {
  BREAKER_LIMIT,
  breakerState,
  bumpBreaker,
  cancelAutomationFires,
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
import { operationErrors, OperationError } from "../../core/errors.ts";
import { closeGithubIssue, deleteRemoteBranch, viewPrInfo } from "../../core/github.ts";
import { lockStatus } from "../../core/locks.ts";
import { createLogger } from "../../core/logger.ts";
import { notifyMacos } from "../../core/notify.ts";
import { StatusKind } from "../../core/types.ts";
import { setBranchTip, toggleGlobalAutomationsPaused } from "../../core/wtstate.ts";
import { watchedBranchTipsQuery, wtStateQuery } from "../../state/queries.ts";

import {
  evaluateAutomations,
  fireIdentity,
  FLEET_SLUG,
  isEligible,
  statusTriggerState,
  type AutomationFire,
  type FireAudience,
} from "../automation-rules.ts";
import { actionSkillPrefix, buildActionVars, isCleanCandidate } from "../app-helpers.ts";
import type { HarnessId } from "../../core/harness/index.ts";
import { useGithubFresh } from "./useGithubFresh.ts";
import type { ActiveSessionGlyph } from "./useHarnessSessions.ts";
import type { LaunchActionOpts, LaunchOutcome } from "./useActionDispatch.ts";
import type { WorktreeRow } from "./useWorktreeRows.ts";
import { cancellableAutomationFires } from "../automation-queue.ts";

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
  persistenceError?: string;
};

type Executing = {
  slug: string;
  /** All slugs the dispatch may touch (quiesceSlugs at dispatch time). */
  slugs: readonly string[];
  kind: "builtin" | "headless" | "session" | "manager";
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

const io = operationErrors("useAutomations");

/** Triggers whose condition needs fresh github data to evaluate at all.
 *  Their breaker resets are gated on freshness too — a boot-stale pass
 *  can't observe "condition cleared". `pr.conflict` is fully local, so
 *  its breaker must reset even when github never fetches this session
 *  (offline, no token), or a trip would wedge forever. */
const GITHUB_DRIVEN: ReadonlySet<AutomationTrigger> = new Set([
  "pr.checks.failed",
  "review_bot.unresolved",
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
  /** Harness whose skill prefix goes into a frozen `{{skill_prefix}}`. */
  primaryHarness: HarnessId;
};

export type AutomationsState = {
  /** True when the config defines any [[automations]] rules. */
  configured: boolean;
  /** Global pause (Shift+A). Persisted in wtstate across restarts. */
  paused: boolean;
  togglePaused: () => Promise<boolean>;
  /** Queued (not yet dispatched) intents, for the title-bar indicator. */
  pendingCount: number;
  clearQueued: () => Effect.Effect<number, OperationError>;
};

/** The breaker/cooldown identity: stacks key on stackId (the target
 *  slug — "first open member" — shifts as members land; the id churns
 *  when the root itself lands, resetting that pair's state). */
function pairTarget(fire: AutomationFire): string {
  return fire.stackId ?? fire.slug;
}

/**
 * Runs that fire on a LANDING and write somewhere outside the
 * worktree. They share three properties, and every one of them is
 * load-bearing below:
 *
 *  - their fire carries everything delivery needs (issue number, branch,
 *    or a frozen var map), so a row that died before dispatch is NOT a
 *    superseded intent;
 *  - they cannot clear the condition that fired them — the branch stays
 *    merged forever — so the breaker must not count them or a couple of
 *    reused-slug landings would trip the rule off;
 *  - they touch nothing in the checkout, so quiescence is meaningless
 *    and their fires carry an empty `quiesceSlugs`.
 *
 * The two builtins are named; a CONFIG action qualifies by declaring
 * `external = true`, which the evaluator turns into a frozen var map.
 * That generalization is the fix for a silent three-day outage: the
 * tracker action has always had all three properties and none of the
 * three exemptions, because the set was a hand-written list of run ids
 * and a list only ever covers what someone remembered. Every merge
 * queued it and every merge dropped it again, `superseded (condition
 * cleared)`, when the `c` sweep archived the row inside the settle
 * window — while `builtin:delete-branch`, queued in the SAME
 * millisecond, ran fine three seconds later. Two intents from one
 * event disagreeing about whether their subject still exists is the
 * tell.
 *
 * A future post-merge run that DOES touch the worktree must not join
 * this set; it would need the quiescence half split back out.
 */
function isPostMergeExternalFire(fire: AutomationFire): boolean {
  return isPostMergeExternalRun(fire.rule.run) || fire.frozenVars !== null;
}

function isPostMergeExternalRun(run: string): boolean {
  return run === "builtin:close-issue" || run === "builtin:delete-branch";
}

/**
 * Does this rule run a SHELL action whose effect leaves the repository?
 * `external` is already the config's word for exactly that, so the
 * property is declared rather than inferred. Shell-only: a prompt
 * action is delivered into a session in the worktree, so it needs the
 * checkout however external its eventual effect.
 */
function isExternalShellRule(rule: AutomationDef): boolean {
  const def = resolveActionDef(rule.run);
  return def?.kind === "shell" && def.external === true;
}

function resolveActionDef(runId: string): ActionDef | null {
  return (
    config.actions.find((d) => d.id === runId) ??
    ALL_BUILTIN_ACTIONS.find((d) => d.id === runId) ??
    null
  );
}

/**
 * Which live conversation a rule's dispatch lands in — the config half
 * of the echo guard in `automation-rules.ts`. Only a prompt action
 * aimed at a persistent session has one: `headless` spawns a fresh run
 * that wrote nothing, and every builtin (notify, clean, restack,
 * post-merge write) talks to the human or to git, neither of which asserts a
 * work status.
 */
function audienceOf(rule: AutomationDef): FireAudience {
  const def = resolveActionDef(rule.run);
  if (!def || def.kind !== "claude") return null;
  return def.target === "manager" || def.target === "session" ? def.target : null;
}

export function useAutomations(opts: AutomationsOpts): AutomationsState {
  const rules = config.automations;
  const configured = rules.length > 0;
  const [pendingCount, setPendingCount] = useState(0);

  const qc = useQueryClient();
  const wtState = useQuery(wtStateQuery());
  // Branches any `branch.advanced` rule watches. Derived from config,
  // which is loaded once at module init, so this list is stable for the
  // life of the process — and empty for every fleet using none, where
  // the query below is disabled and costs nothing.
  const watchedBranches = useMemo(
    () => [
      ...new Set(
        (config.automations ?? [])
          .filter((r) => r.on === "branch.advanced" && r.branch)
          .map((r) => r.branch!),
      ),
    ],
    [],
  );
  const branchTipsQ = useQuery(watchedBranchTipsQuery(watchedBranches));
  // Record a watched branch the first time it resolves, WITHOUT firing.
  // Without this the trigger could never fire at all: the evaluator
  // needs a previous tip to form a range, and nothing else ever writes
  // the first one. Doing it here rather than in the evaluator keeps
  // that module a pure function of its inputs, and `setBranchTip`
  // no-ops when the value is unchanged, so this settles after one pass.
  useEffect(() => {
    const seen = wtState.data?.branchTips;
    if (!seen || !branchTipsQ.data) return;
    for (const [branch, sha] of Object.entries(branchTipsQ.data)) {
      if (seen[branch] === undefined) {
        setBranchTip(branch, sha);
        log.event.dim(`watching ${branch} from ${sha.slice(0, 7)}`);
      }
    }
  }, [branchTipsQ.data, wtState.data?.branchTips]);
  // Pair each watched branch's CURRENT tip with the last one wt
  // recorded. Both halves must come from the same pass: comparing a
  // fresh tip against a watermark read at some other moment is the
  // reference-frame mistake that makes a range either repeat or vanish.
  const branchTips = useRef<
    ReadonlyMap<string, { now: string; seen: string | null }>
  >(new Map());
  branchTips.current = useMemo(() => {
    const seen: Record<string, string> = wtState.data?.branchTips ?? {};
    const out = new Map<string, { now: string; seen: string | null }>();
    for (const b of watchedBranches) {
      const now = branchTipsQ.data?.[b];
      if (now) out.set(b, { now, seen: seen[b] ?? null });
    }
    return out;
  }, [watchedBranches, branchTipsQ.data, wtState.data?.branchTips]);
  // Global pause lives in wtstate (persisted across restarts, toggled
  // via Shift+A). Until the state file loads, treat as paused — the
  // engine must not fire before it knows the pause flags.
  // WT_AUTOMATIONS=off pins the engine paused for the whole process:
  // the TUI test harness (scripts/tui-test.sh) sets it so probe
  // instances never dispatch alongside the user's live instance.
  const wtStateReady = wtState.data !== undefined;
  const paused =
    process.env.WT_AUTOMATIONS === "off" ||
    !wtStateReady ||
    wtState.data.automationsPaused === true;
  // Freshness subscription extracted to `useGithubFresh` (same
  // persisted-cache hard rule).
  const githubFresh = useGithubFresh(configured);

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
    primaryHarness: opts.primaryHarness,
    githubFresh,
    pausedSlugs: new Set<string>(),
    paused,
    stateReady: wtStateReady,
  });
  // Effective per-worktree pause set: individually-paused slugs plus
  // every member of a paused STACK (Ctrl+A on any stack row pauses by
  // stackId, so members stacked on later are covered too; the toggle
  // also mirrors per-slug flags so the pause survives a re-root).
  const pausedSlugs = new Set<string>();
  for (const [slug, st] of Object.entries(wtState.data?.slugs ?? {})) {
    if (st.automationsPaused === true) pausedSlugs.add(slug);
  }
  // Archived rows count too. A post-merge `external` run is exempted
  // from the row's death on purpose, so the slugs whose automations are
  // still live are precisely the ones with no per-slug entry left to
  // carry a pause — read it off the removed history instead.
  for (const e of wtState.data?.removed ?? []) {
    if (e.automationsPaused === true) pausedSlugs.add(e.slug);
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
    primaryHarness: opts.primaryHarness,
    githubFresh,
    pausedSlugs,
    paused,
    stateReady: wtStateReady,
  };

  const intents = useRef<Map<string, Intent>>(new Map());
  const executing = useRef<Map<string, Executing>>(new Map());
  const passFiber = useRef<Fiber.Fiber<void, never> | null>(null);
  const dispatchFibers = useRef(new Set<Fiber.Fiber<void, never>>());
  const automationActive = useRef(true);

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
      // A member archived for cleanup but whose `_destroy` child hasn't
      // grabbed the flock yet reads as unlocked and not-Busy — block on
      // `archived` directly so a dispatch can't slip into that window.
      if (row.archived) return `${slug} is being cleaned up`;
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
        // The delivered prompt runs invisibly inside the session; hold
        // the slot while it's observably working, with a hard cap so a
        // long manual session can't pin the slot forever.
        return (
          sessionBusyState(ex.slug) === null ||
          now - ex.dispatchedAt > SESSION_SLOT_MAX_MS
        );
      case "manager":
        // The manager isn't a worktree row — `sessionBusyState` (built
        // from rows) can't see it, and polling the SOURCE slug would
        // hold the slot hostage to a session that has nothing to do
        // with the delivery. The injection completing (promiseDone,
        // checked above) IS the release; the manager works on after.
        return true;
      default: {
        const _exhaustive: never = ex.kind;
        void _exhaustive;
        return true;
      }
    }
  }

  const execute = Effect.fn("execute")(function* (
    fire: AutomationFire,
  ): Effect.fn.Return<ExecuteOutcome, OperationError> {
    const { rule, slug, stackId } = fire;
    const wtLog = createLogger(slug);
    if (rule.run === "builtin:restack") {
      if (!stackId) {
        return yield* new OperationError({
          source: "useAutomations",
          operation: "restack",
          cause: "builtin:restack fire without a stackId",
        });
      }
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
        yield* io.promise("clean merged members", () => latest.current.doCleanSlugs(mergedSlugs));
      }
      // Target the restack at a SURVIVING member's branch, never the
      // stack id: the id is the ROOT's branch, and when the merged
      // member is the root (the common bottom-up landing) the pre-clean
      // just destroyed that branch — it no longer resolves a chain.
      // `fire.slug` is the first open member, which the pre-clean never
      // touches; the engine resolves the whole surviving stack from it.
      const targetRow = latest.current.rows.find((r) => r.wt.slug === slug);
      const outcome = yield* io.promise(
        "restack stack",
        () => latest.current.doRestackStack(targetRow?.wt.branch ?? stackId),
      );
      if (outcome === "busy") {
        // Lost the mutex in the window between the peek above and the
        // engine acquiring it (a manual `R` mid-dispatch). The merged
        // members are already cleaned, so surface it as a failure that
        // names the manual follow-up instead of silently retrying.
        return yield* new OperationError({
          source: "useAutomations",
          operation: "restack",
          cause:
            "restack engine grabbed by another run after the pre-clean — press R (or /restack) once it's free",
        });
      }
      return { declined: null };
    }
    if (rule.run === "builtin:clean") {
      yield* io.promise("clean worktree", () => latest.current.doCleanSlugs([slug]));
      return { declined: null };
    }
    if (rule.run === "builtin:notify") {
      // The attention feed already narrates the transition; this is the
      // "you're not looking at wt" leg. Detail carries state + note.
      yield* notifyMacos(`wt · ${slug}`, fire.detail);
      return { declined: null };
    }
    if (rule.run === "builtin:close-issue") {
      // The number rode the fire, frozen at evaluation — delivery must
      // not depend on the row or its wtstate entry surviving (a racing
      // clean/restack destroys the row, and a recreated slug would
      // even offer up the WRONG state to a live re-read).
      const issue = fire.closeIssue;
      if (issue === null) {
        wtLog.event.dim(`auto ${rule.id}: fire carried no issue number — nothing to close`);
        return { declined: null };
      }
      const r = yield* closeGithubIssue(issue);
      if (r.ok) {
        // ATTENTION, not the firehose: this is the one builtin that
        // writes to a system OUTSIDE wt, where wt's undo does not
        // reach. Every other automation touches a worktree the human
        // can see on the board; a closed GitHub issue is invisible here
        // and stays closed until somebody notices. It earns a line the
        // human actually reads, plus the toast that comes with it.
        wtLog.attention.info(`auto ${rule.id}: closed issue #${issue}`);
      } else {
        // Deliberately non-fatal: repos whose PR bodies carry closing
        // keywords race us to it, and "already closed" (or any other
        // refusal) changes nothing. Log, never retry.
        wtLog.event.dim(`auto ${rule.id}: close issue #${issue}: ${r.error}`);
      }
      return { declined: null };
    }
    if (rule.run === "builtin:delete-branch") {
      // Frozen onto the fire at evaluation, same as close-issue: the
      // row is routinely gone by delivery, and a live re-read could
      // resolve a recreated slug to a branch that has NOT landed.
      const branch = fire.deleteBranch;
      if (branch === null) {
        wtLog.event.dim(`auto ${rule.id}: fire carried no branch — nothing to delete`);
        return { declined: null };
      }
      // CONFIRM before deleting. The condition that produced this fire
      // is derived from cached state, and deleting a remote ref is the
      // one effect here wt cannot undo — GitHub also CLOSES any open PR
      // whose head ref disappears, with no close event to explain it,
      // so a wrong delete destroys a PR and hides why. A live read is
      // one gh call on a path that runs once per merged branch.
      const live = yield* viewPrInfo(branch);
      const confirmed = live
        ? live.state === "MERGED"
        // No PR on the branch at all: only the fire that claimed local
        // containment may proceed. A fire that NAMED a PR and now finds
        // none cannot confirm its own claim, and unknown is not fine.
        : fire.deleteBranchPr === null;
      if (!confirmed) {
        const why = live
          ? `#${live.number} is ${live.state.toLowerCase()}, not merged`
          : `no PR found for it, and the fire claimed #${fire.deleteBranchPr}`;
        // Attention, not the firehose: this is wt declining to act on
        // its own conclusion, which means something upstream produced a
        // merged verdict for an unmerged branch and is worth a look.
        wtLog.attention.warn(
          `auto ${rule.id}: NOT deleting ${branch} — ${why}`,
        );
        return { declined: null };
      }
      const r = yield* deleteRemoteBranch(branch);
      if (r.ok) {
        // ATTENTION for the same reason close-issue takes it: this
        // writes to a system outside wt, where wt's undo does not
        // reach and the board shows nothing.
        wtLog.attention.info(`auto ${rule.id}: deleted remote branch ${branch}`);
      } else {
        // Non-fatal and usually not even a failure: a repo with
        // GitHub's own "automatically delete head branches" enabled,
        // or anyone deleting it by hand, gets there first and GitHub
        // answers "Reference does not exist". That is the end state we
        // wanted. Log, never retry.
        wtLog.event.dim(`auto ${rule.id}: delete branch ${branch}: ${r.error}`);
      }
      return { declined: null };
    }
    const def = resolveActionDef(rule.run);
    // A typed failure, so the caller's ledger/breaker bookkeeping and the
    // error toast see it; a thrown Error would be a defect that skips both.
    if (!def) {
      return yield* new OperationError({
        source: "useAutomations",
        operation: "resolve action",
        cause: `action "${rule.run}" not found in config`,
      });
    }
    // A fleet-level fire belongs to no worktree, so it cannot go
    // through `launchAction` (row guards, row template vars, a row
    // cwd). It runs in the MAIN CLONE — the only checkout that is
    // guaranteed to exist, and the one whose refs the range was
    // measured against.
    const range = fire.branchRange;
    if (range) {
      const started = yield* actionRegistry.start(
        def,
        FLEET_SLUG,
        config.paths.mainClone,
        "",
        { branch: range.branch, from: range.from, to: range.to },
        "claude",
        { autoFireKeys: fire.fireKeys },
      );
      if (!started.ok) return { declined: started.reason };
      // Advance the watermark only now. The range is consumed exactly
      // once, so moving the mark before the run is launched would drop
      // it with nothing anywhere to say so — and moving it on mere
      // OBSERVATION would drop every range whose dispatch was declined.
      setBranchTip(range.branch, range.to);
      return { declined: null };
    }
    // A post-merge EXTERNAL run cannot go through `launchAction`, for
    // the same reason a fleet-level one cannot: that path resolves the
    // ROW — its guards, its template vars, its cwd — and the row is
    // exactly what a landing destroys. It runs in the main clone off
    // the vars frozen at fire time, which is the only checkout
    // guaranteed to still be there. (A worktree cwd that no longer
    // exists is not a soft failure: `Bun.spawn` rejects a bad cwd
    // before it ever reaches the binary, so the run would report an
    // exit code without having run.)
    const frozenVars = fire.frozenVars;
    if (frozenVars) {
      const started = yield* actionRegistry.start(
        def,
        slug,
        config.paths.mainClone,
        "",
        frozenVars,
        "claude",
        { autoFireKeys: fire.fireKeys },
      );
      return { declined: started.ok ? null : started.reason };
    }
    const outcome = yield* io.promise(
      "launch action",
      () => latest.current.launchAction(slug, def, "", undefined, {
        autoFireKeys: fire.fireKeys,
      }),
    );
    // A launch the guards refused (action already running, busy row,
    // unmet requirements at the last instant) never ran — un-consume.
    return {
      declined: outcome.launched ? null : (outcome.reason ?? "launch declined"),
    };
  });

  function dispatchKind(rule: AutomationFire["rule"]): Executing["kind"] {
    if (rule.run.startsWith("builtin:")) return "builtin";
    const def = resolveActionDef(rule.run);
    if (def?.kind === "claude" && def.target === "manager") return "manager";
    if (def?.kind === "claude" && def.target === "session") return "session";
    return "headless";
  }

  function evaluationContext() {
    const ctx = latest.current;
    return {
      githubFresh: ctx.githubFresh,
      isPausedSlug: (slug: string) => ctx.pausedSlugs.has(slug),
      audienceOf,
      externalOf: isExternalShellRule,
      varsFor: (rule: AutomationDef, row: WorktreeRow) => {
        const def = resolveActionDef(rule.run);
        return buildActionVars(row, actionSkillPrefix(def, ctx.primaryHarness));
      },
      branchTips: branchTips.current,
      nowMs: Date.now(),
    };
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

    const evalCtx = evaluationContext();
    const fires = evaluateAutomations(rules, ctx.rows, evalCtx);
    const byId = new Map(fires.map((f) => [fireIdentity(f), f] as const));

    // Breaker resets: a (rule, target) with breaker state whose
    // condition is now observed FALSE means the failure actually
    // cleared — the consecutive count starts over. Github-driven
    // triggers gate the reset on freshness (a boot-stale pass can't
    // observe "cleared"); purely local ones (pr.conflict) reset
    // unconditionally, or an offline session could wedge a trip
    // forever.
    //
    // Eligibility gates ALL resets: the evaluator skips ineligible rows
    // (archived, Busy lock, paused) before ever testing the condition,
    // so an absent fire for one is "not evaluated", not "cleared" —
    // resetting there would let a Ctrl+A toggle or a restack's own lock
    // window hand a flapping fix-loop free strikes. For stacks the fire
    // targets the stackId, so ANY ineligible member blanks that stack's
    // reset for the pass (the reset re-runs next pass; a missed one is
    // harmless, a wrong one defeats the breaker).
    const ineligibleStacks = new Set<string>();
    for (const row of ctx.rows) {
      if (row.stack && !isEligible(row, evalCtx)) {
        ineligibleStacks.add(row.stack.stackId);
      }
    }
    for (const rule of rules) {
      if (GITHUB_DRIVEN.has(rule.on) && !ctx.githubFresh) continue;
      if (rule.on === "stack.parent_merged") {
        for (const row of ctx.rows) {
          const sid = row.stack?.stackId;
          if (!sid || ineligibleStacks.has(sid)) continue;
          if (!byId.has(`${rule.id}|${sid}`)) resetBreaker(rule.id, sid);
        }
      } else {
        const statusWant = statusTriggerState(rule.on);
        for (const row of ctx.rows) {
          if (!isEligible(row, evalCtx)) continue;
          // `pr.conflict` on a PR-carrying row is freshness-gated in the
          // evaluator (a conflict can't be read off persisted cache), so
          // an absent fire before the first github fetch is "unknown",
          // not "cleared" — resetting here would wipe a PERSISTED trip on
          // every boot-stale pass and hand the auto-fix loop two free
          // strikes. A PR-less row's check is purely local and always
          // observable, so it still resets unconditionally (the offline-
          // wedge case the unconditional reset exists for).
          if (rule.on === "pr.conflict" && row.pr && !ctx.githubFresh) continue;
          // Status triggers: double-gate on the asserted state itself —
          // it's directly observable regardless of row eligibility.
          if (statusWant && row.work?.state === statusWant) continue;
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
    // Post-merge external intents get a narrower rule: a merge can't
    // un-happen, and the row routinely dies right after one (a clean or
    // restack pre-clean archives at dispatch; a manual `c` easily beats
    // the 10s settle window), so "row gone/archived/busy" must NOT count
    // as superseded — the run still has to happen, off the value frozen
    // into the fire. Only a genuine clear (the row still
    // evaluable but no longer firing — e.g. the issue was detached)
    // or an explicit per-slug pause drops one.
    for (const [id, intent] of intents.current) {
      if (byId.has(id)) continue;
      if (isPostMergeExternalFire(intent.fire)) {
        const row = ctx.rows.find((r) => r.wt.slug === intent.fire.slug);
        const cleared = row !== undefined && isEligible(row, evalCtx);
        if (!cleared && !evalCtx.isPausedSlug(intent.fire.slug)) continue;
      }
      if (intent.announced) {
        createLogger(intent.fire.slug).event.dim(
          `auto ${intent.fire.rule.id}: superseded (condition cleared) — dropped`,
        );
      }
      intents.current.delete(id);
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
    let managerInFlight = false;
    for (const ex of executing.current.values()) {
      for (const s of ex.slugs) occupiedSlugs.add(s);
      if (ex.isRestack && ex.stackId) restackStacksInFlight.add(ex.stackId);
      if (ex.kind === "manager") managerInFlight = true;
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
      const ruleDef = rule.run.startsWith("builtin:")
        ? null
        : resolveActionDef(rule.run);
      const isManagerRun = ruleDef?.kind === "claude" && ruleDef.target === "manager";
      // The breaker exists to stop a fix-it action from hammering a
      // condition it keeps failing to clear. Notifications, manager
      // briefings, and issue closes don't CLEAR anything — the
      // condition legitimately stays true until the human (or the
      // clean flow) acts — so counting them would swallow the 3rd+
      // needs-human ping exactly when it matters, or trip a post-merge
      // run after a couple of reused-slug landings. Cooldowns still apply
      // for spacing.
      const breakerExempt =
        rule.run === "builtin:notify" ||
        isPostMergeExternalFire(fire) ||
        isManagerRun;
      if (fire.quiesceSlugs.some((s) => occupiedSlugs.has(s))) continue;
      if (
        isRestack &&
        fire.stackId !== null &&
        (restackStacksInFlight.has(fire.stackId) ||
          ctx.isRestackBusy(fire.stackId))
      ) {
        continue;
      }
      // One manager send at a time. The session is a shared singleton;
      // this gate keeps the queue orderly across every harness transport.
      if (isManagerRun && managerInFlight) continue;
      if (!intent.announced) {
        intent.announced = true;
        const settleLeft = Math.ceil(
          Math.max(0, rule.settleSeconds * 1000 - (now - intent.createdAt)) / 1000,
        );
        const settleNote = settleLeft > 0 ? ` (${settleLeft}s settle remaining)` : "";
        wtLog.attention.info(`auto ${rule.id}: ${fire.detail} · queued${settleNote}`, { toast: false });
      }
      const target = pairTarget(fire);
      const breaker = breakerState(rule.id, target);
      if (!breakerExempt && breaker.trippedAt !== null) {
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
      // Notifications and manager briefings never touch the worktree,
      // so quiescence is meaningless for them — and a needs-human fire
      // happens exactly while the session is non-quiescent (asking).
      // Bypass, don't wait. The two flags coincide today because every
      // exempt run is exempt for the same reason (it can't clear or
      // disturb anything in the worktree — the post-merge runs' fires
      // even carry an empty quiesceSlugs); a future run that's breaker-
      // exempt but does touch the worktree must split them.
      const bypassQuiesce = breakerExempt;
      const blocked = bypassQuiesce ? null : quiesceBlockReason(fire, now);
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
      const def = ruleDef;
      if (def) {
        const row = ctx.rows.find((r) => r.wt.slug === fire.slug);
        // A FROZEN fire is evaluated against its frozen values, never
        // against the row: the row is the thing a landing destroys,
        // and reading `row?.issueId` off a swept worktree answers
        // "no tracker id" about a run that is holding one.
        const avail = evaluateActionRequirements(
          def.requires,
          fire.frozenVars
            ? {
                slug: fire.slug,
                issueId: fire.frozenVars.issue_id ?? null,
                pr: fire.frozenPr,
                deployed: false,
              }
            : {
                slug: fire.slug,
                issueId: row?.issueId,
                pr: row?.pr,
                deployed: row?.fields.deploy.data ?? false,
              },
        );
        if (!avail.ok) {
          // "Keep it pending, the row may change" is right for a
          // row-backed fire and is a LEAK for a frozen one: its inputs
          // cannot change by construction, so the answer is the same
          // on every future pass and the intent would sit in the queue
          // for the life of the process — and the supersede guard that
          // now protects it from the row's death is exactly what stops
          // anything else clearing it. Drop it, and say why once.
          if (fire.frozenVars) {
            markFiresDelivered(fire.fireKeys);
            intents.current.delete(intent.id);
            wtLog.event.dim(`auto ${rule.id}: ${avail.reason} — skipped`);
          }
          continue;
        }
      }
      if (!breakerExempt && breaker.count >= BREAKER_LIMIT) {
        tripBreaker(rule.id, target);
        markFiresDelivered(fire.fireKeys);
        wtLog.event.err(
          `auto ${rule.id} tripped breaker on ${target} — ${BREAKER_LIMIT} runs, condition never cleared; fix by hand to re-arm`,
          { toast: true },
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
      try {
        if (!markFiresDispatched(fire.fireKeys, rule.id, target)) {
          intents.current.delete(intent.id);
          continue;
        }
      } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        if (intent.persistenceError !== message) {
          wtLog.attention.err(`auto ${rule.id}: not started; cannot persist dispatch: ${message}`);
          intent.persistenceError = message;
        }
        continue;
      }
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
      if (isManagerRun) managerInFlight = true;
      wtLog.attention.info(`auto ${rule.id}: ${fire.detail} · running ${rule.run}`, {
        toast: true,
      });
      let dispatchFiber: Fiber.Fiber<void, never>;
      const dispatch = execute(fire).pipe(
        Effect.match({
          onSuccess: (outcome) => {
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
            if (!breakerExempt) bumpBreaker(rule.id, target);
          },
          onFailure: (error) => {
            // A run that LAUNCHED and failed does NOT retry: keys stay
            // handled; a new push (new fire key) is the sanctioned
            // retry path.
            markFiresDelivered(fire.fireKeys);
            if (!breakerExempt) bumpBreaker(rule.id, target);
            wtLog.event.err(`auto ${rule.id} failed: ${error.message}`, { toast: true });
          },
        }),
        Effect.ensuring(
          Effect.sync(() => {
            dispatchFibers.current.delete(dispatchFiber);
            if (!automationActive.current) return;
            entry.promiseDone = true;
            schedulePass();
          }),
        ),
      );
      dispatchFiber = Effect.runFork(dispatch);
      dispatchFibers.current.add(dispatchFiber);
    }

    setPendingCount(intents.current.size);
  }

  function schedulePass(): void {
    if (passFiber.current) return;
    let fiber: Fiber.Fiber<void, never>;
    fiber = Effect.runFork(
      Effect.sleep(`${PASS_DEBOUNCE_MS} millis`).pipe(
        Effect.andThen(
          Effect.sync(() => {
            if (passFiber.current === fiber) passFiber.current = null;
            runPass();
          }),
        ),
        Effect.ensuring(
          Effect.sync(() => {
            if (passFiber.current === fiber) passFiber.current = null;
          }),
        ),
      ),
    );
    passFiber.current = fiber;
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
    automationActive.current = true;
    const heartbeat = Effect.runFork(
      Effect.forever(
        Effect.sleep(`${TICK_MS} millis`).pipe(
          Effect.andThen(Effect.sync(runPass)),
        ),
      ),
    );
    return () => {
      automationActive.current = false;
      Effect.runFork(Fiber.interrupt(heartbeat));
      if (passFiber.current) {
        Effect.runFork(Fiber.interrupt(passFiber.current));
        passFiber.current = null;
      }
      for (const fiber of dispatchFibers.current) {
        Effect.runFork(Fiber.interrupt(fiber));
      }
      dispatchFibers.current.clear();
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
    clearQueued: Effect.fn("clearQueuedAutomations")(function* () {
      const pending = cancellableAutomationFires({
        paused: latest.current.paused,
        stateReady: latest.current.stateReady,
        rules,
        rows: latest.current.rows,
        evalCtx: evaluationContext(),
        pending: [...intents.current.values()].map((intent) => intent.fire),
        executing: new Set(executing.current.keys()),
        handled: hasHandledFire,
      });
      yield* cancelAutomationFires(pending.flatMap((fire) => fire.fireKeys));
      for (const fire of pending) intents.current.delete(fireIdentity(fire));
      setPendingCount(intents.current.size);
      log.info("cancelled queued automations", { count: pending.length });
      return pending.length;
    }),
  };
}
