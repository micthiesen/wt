import type { ActionLine, ToolStartMap } from "../harness/claude/events.ts";
import type { ActionTailHandle } from "../action-tail.ts";
import type { EffectTag } from "../config.ts";
import type { PullRequest } from "../types.ts";

/**
 * Snapshot of row state the requirement predicates read. Kept narrow
 * so callers can pass a row from the TUI aggregator or a synthesized
 * shape from a one-off context without dragging in `WorktreeRow`'s
 * full surface.
 */
export type ActionRowState = {
  pr: PullRequest | undefined;
  /**
   * `isOurStageDeployed` result for the row. Strict gate (matches
   * the safe-stage rules); used by `requires: ["deployed"]` actions
   * (e.g. a user-configured `pnpm sst remove --stage {{stage}}`).
   */
  deployed: boolean;
};

export type ActionAvailability =
  | { ok: true }
  | { ok: false; reason: string };

/**
 * Per-launch substitution map for action templates. Keys are bare names
 * (no braces); the renderer wraps them as `{{name}}` when scanning.
 *
 * Currently produced at `tui/app.tsx`'s `launchAction` from the row +
 * config. The full set: `base`, `base_branch`, `branch`, `slug`, `cwd`,
 * `pr`, `stage`, `arg` (when the action collects one), and
 * `skill_prefix` (the harness skill-invocation prefix — `/` for Claude
 * Code, `$` for OpenCode / Codex; see `actionSkillPrefix` in
 * `tui/app.tsx` for how the target harness is picked per launch).
 * Kept loose (`Record<string, string>`) so adding a new var is a
 * one-liner at the callsite — no schema dance.
 */
export type ActionVars = Readonly<Record<string, string>>;

export type ActionStatus = "running" | "succeeded" | "failed" | "killed";
export type ActionRunKind = "claude" | "shell" | "harness";

export type ActionRun = {
  slug: string;
  /**
   * Runtime output parser kind. Claude headless actions use stream-json;
   * shell and non-Claude headless harnesses are tailed as raw text.
   */
  kind: ActionRunKind;
  actionId: string;
  actionName: string;
  prompt: string;
  startedAt: number;
  endedAt?: number;
  status: ActionStatus;
  lines: readonly ActionLine[];
  /** Per-run dir under `<logDir>/actions/`. Holds meta.json,
   *  stream.log, stderr.log, and (when terminal) done.json. */
  runDir: string;
  /**
   * State domains this run mutates. Snapshot of the def's `affects` at
   * start time so a later config edit can't change which invalidations
   * fire for an in-flight run. Consumed by the TUI subscriber that
   * dispatches cache invalidations when the run reaches a terminal
   * status — see the architecture block in `state/hooks.ts`.
   */
  affects: readonly EffectTag[];
  /**
   * Fire keys of the automation dispatch that launched this run, when
   * it was auto-launched (absent for manual runs). Persisted in
   * meta.json so the automation ledger's boot reconciliation can match
   * a `dispatched` entry against a run that really launched — see
   * `reconcileDispatchedFires` in `core/automations.ts`.
   */
  autoFireKeys?: readonly string[];
};

export type ActionStartResult =
  | { ok: true; run: ActionRun }
  | { ok: false; reason: string };

export type Listener = () => void;

/** On-disk shape of `meta.json`. Fields beyond the run identity may
 *  be missing in older runs; the loader tolerates that and reconstructs
 *  conservatively. */
export type ActionMeta = {
  version: 1;
  slug: string;
  runId: string;
  kind: ActionRunKind;
  actionId: string;
  actionName: string;
  prompt: string;
  affects: readonly EffectTag[];
  autoFireKeys?: readonly string[];
  startedAt: number;
  endedAt?: number;
  exitCode?: number;
  status: ActionStatus;
};

export type LiveHandles = {
  tail: ActionTailHandle;
  done: ActionTailHandle;
  /** Per-run map of tool_use_id → call metadata. Survives across the
   *  boot-seed → live-tail handoff so result-event durations are
   *  computed against the same start record. */
  toolStarts: ToolStartMap;
  /** True once the stream-json `result` event has been processed for
   *  this run; prevents the done.json handler from synthesizing a
   *  duplicate exit line. */
  resultEventSeen: boolean;
};
