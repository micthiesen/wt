/**
 * Pure, App-state-free helpers extracted from `app.tsx`: key-event
 * classification, prompt-input parsing, action template vars, and row
 * predicates. Everything here takes its inputs explicitly — nothing
 * closes over React state — which is what makes it safe to house
 * outside the component without changing behavior.
 */
import { existsSync } from "node:fs";

import type { ActionDef, ActionLine, ActionVars } from "../core/actions.ts";
import { config } from "../core/config.ts";
import { getHarness, type HarnessId } from "../core/harness/index.ts";
import { lockLabel, lockStatus } from "../core/locks.ts";
import { canEnterSessionDuringLock } from "../core/session-readiness.ts";
import { expectedStage } from "../core/stage-safety.ts";
import { StatusKind } from "../core/types.ts";

import type { WorktreeRow } from "./hooks/useWorktreeRows.ts";

/**
 * Resolve the diff base ref for a worktree row. Same priority chain
 * as `useWorktreeRows.resolveStackedOn` exposes: a parent branch when
 * the row is stack-detected or its PR targets a non-trunk base,
 * otherwise `origin/<config.branch.base>`. Used by the F11 handler to
 * fill `{{base}}` in `[diff].command` and by the kill-on-base-change
 * effect to detect when a stacked row's parent moved.
 *
 * Returns the raw ref — for a stack-on-stack root this can be an external
 * parent branch that's since been merged + cleaned (dead). Callers that
 * shell out against it (the F11 diff session) pass it through
 * `effectiveBaseOrTrunk` first so a dead base degrades to trunk; the
 * string-compare consumers (kill-on-base-change) don't care.
 */
export function resolveDiffBase(row: WorktreeRow): string {
  return row.stackedOn?.diffBase ?? `origin/${config.branch.base}`;
}

/**
 * Match a plain lowercase-letter binding — name equals `letter` and no
 * modifier keys are held. The naive `k.name === "<letter>"` is a trap:
 * the parser lowercases letter names and exposes `k.shift` separately,
 * so without this guard `Shift+L` (and modified variants like Hyper+L)
 * fire the lowercase action, which is almost always wrong. Action
 * bindings (open-zed, archive, …) should always go through here.
 * Navigation arrows are checked separately upstream where Shift+arrow
 * scrolling is intentional.
 */
export function isPlainLetter(
  k: {
    name: string;
    shift: boolean;
    ctrl: boolean;
    option: boolean;
    super?: boolean;
    hyper?: boolean;
    meta: boolean;
  },
  letter: string,
): boolean {
  return (
    k.name === letter &&
    !k.shift &&
    !k.ctrl &&
    !k.option &&
    !k.super &&
    !k.hyper &&
    !k.meta
  );
}

/**
 * Plain Shift+letter guard — shift is the only modifier held. Used
 * by the section move/rename bindings (J/K/L). Excludes Hyper
 * explicitly because the kitty keyboard protocol exposes that as a
 * separate flag and Caps Lock-mapped Hyper layouts include shift in
 * the four-modifier combo, which would otherwise leak into these.
 */
export function isShiftedLetter(
  k: {
    name: string;
    shift: boolean;
    ctrl: boolean;
    option: boolean;
    super?: boolean;
    hyper?: boolean;
  },
  letter: string,
): boolean {
  return (
    k.name === letter &&
    k.shift &&
    !k.ctrl &&
    !k.option &&
    !k.super &&
    !k.hyper
  );
}

/**
 * Filter a key sequence down to printable ASCII so single keypresses
 * and pasted blobs both append cleanly, while control chars (escape,
 * backspace, embedded newlines from multi-line pastes) drop out.
 */
export function printableText(sequence: string | undefined): string {
  if (!sequence) return "";
  // Escape-sequence keypresses (function/arrow/nav keys — F10 arrives as
  // "\x1b[21~", arrows as "\x1b[A") lead with ESC. Stripping control chars
  // alone would leak the printable tail ("[21~", "[A") into the text, so
  // bail on a leading ESC outright — real typed text and pastes never
  // start with one.
  if (sequence.charCodeAt(0) === 0x1b) return "";
  let out = "";
  for (let i = 0; i < sequence.length; i++) {
    const ch = sequence[i]!;
    if (ch >= " " && ch <= "~") out += ch;
  }
  return out;
}

/**
 * Like `printableText`, but preserves `\n` and `\t` so multi-line code
 * snippets paste cleanly into the action-edit textarea — single-line
 * new-worktree / rename inputs still use `printableText`.
 */
export function printableMultiline(sequence: string | undefined): string {
  if (!sequence) return "";
  // Same escape-sequence guard as `printableText` (see there): a leading
  // ESC means a function/arrow/nav key, not text — drop it whole.
  if (sequence.charCodeAt(0) === 0x1b) return "";
  let out = "";
  for (let i = 0; i < sequence.length; i++) {
    const ch = sequence[i]!;
    if (ch === "\n" || ch === "\t" || (ch >= " " && ch <= "~")) out += ch;
  }
  return out;
}

export type NewInput =
  | { input: string; anyAuthor: boolean; base?: string }
  | { error: string };

/**
 * Parse the TUI's `new:` prompt value: one positional arg
 * (linear-id | branch | slug), plus optional `--any` / `--base <ref>`.
 * Mirrors `wt new` so muscle memory carries over. A `defaultBase` from
 * the `N` keybinding seeds the base; an explicit `--base` overrides.
 */
export function parseNewInput(raw: string, defaultBase?: string): NewInput {
  const tokens = raw.trim().split(/\s+/).filter(Boolean);
  let input: string | undefined;
  let anyAuthor = false;
  let base = defaultBase;
  for (let i = 0; i < tokens.length; i++) {
    const t = tokens[i]!;
    if (t === "--any") {
      anyAuthor = true;
    } else if (t === "--base") {
      const next = tokens[++i];
      if (!next) return { error: "--base requires a ref" };
      base = next;
    } else if (t.startsWith("--")) {
      return { error: `unknown flag: ${t}` };
    } else if (input === undefined) {
      input = t;
    } else {
      return { error: `unexpected arg: ${t}` };
    }
  }
  if (!input) return { error: "missing input" };
  return { input, anyAuthor, base };
}

/**
 * A worktree is safe to clean when the branch is finished upstream. We
 * accept three signals — local "merged into main", local "[gone]" after
 * a fetch+prune, or the PR itself being merged. The PR check catches
 * squash-merged branches before the next `R` lands, which is by far the
 * most common case with GitHub's default merge style.
 */
export function isCleanCandidate(row: WorktreeRow): boolean {
  // Archived worktrees opted out of the automatic lifecycle — don't
  // sweep them even if their branch has merged since.
  if (row.archived) return false;
  if (row.status.kind === StatusKind.Busy) return false;
  if (row.status.kind === StatusKind.Merged) return true;
  if (row.status.kind === StatusKind.Gone) return true;
  if (row.pr?.state === "MERGED") return true;
  return false;
}

/**
 * Reason a worktree can't accept a new action right now, or null when it's
 * free. Checks the archived flag (a clean
 * / destroy tucks the row into the archived section the instant it
 * dispatches) and the authoritative on-disk flock (a remove/init/
 * restack in flight). Both beat the cached `row.status.kind`, which lags
 * a just-dispatched background remove by ~600ms (the fs-watch → debounce
 * → refetch cycle). Actions retain this strict gate because they may require
 * installed dependencies or mutate files while setup is still running.
 */
export function launchBlockedReason(row: WorktreeRow): string | null {
  if (row.archived) return "being cleaned up";
  const lock = lockStatus(row.wt.slug);
  return lock ? lockLabel(lock) : null;
}

/**
 * Session-specific lock gate. F10/F11/F12 are safe as soon as init has
 * materialized the checkout, even while env setup or pnpm install continues.
 * Other live locks remain blocked so a session cannot race removal/restacking.
 */
export function sessionLaunchBlockedReason(row: WorktreeRow): string | null {
  if (row.archived) return "being cleaned up";
  const lock = lockStatus(row.wt.slug);
  return canEnterSessionDuringLock(lock, existsSync(row.wt.path))
    ? null
    : lockLabel(lock!);
}

/**
 * Scan an ActionRun's captured lines with the action's `label_extract`
 * regex. Latest per-line match wins, capture group 1 becomes the label
 * (falls back to the full match when the pattern has no group); the
 * result is trimmed. Returns null when the def has no extractor, the
 * pattern fails to compile, or nothing matched. Compile is per-call —
 * runs once per terminal action, so caching wouldn't buy much.
 */
export function extractLabel(
  lines: readonly ActionLine[],
  pattern: string | null,
): string | null {
  if (!pattern) return null;
  let re: RegExp;
  try {
    re = new RegExp(pattern);
  } catch {
    return null;
  }
  let found: string | null = null;
  for (const line of lines) {
    const m = re.exec(line.text);
    if (m) found = (m[1] ?? m[0]).trim() || null;
  }
  return found;
}

/**
 * Variables exposed to action templates as `{{name}}`. Kept in the TUI
 * layer (not `core/actions.ts`) because it depends on `WorktreeRow`,
 * which is a TUI-layer type — the registry stays UI-agnostic.
 *
 * `base` mirrors the details-pane base value (may be a SHA when the
 * stack signal is `patch-id`); `base_branch` is always a named ref —
 * the right thing to plug into `git rebase` or a "rebase on X" prompt.
 */
export function buildActionVars(row: WorktreeRow, skillPrefix: string): ActionVars {
  const baseBranch = row.stackedOn?.branch ?? config.branch.base;
  const base = row.stackedOn?.diffBase ?? config.branch.base;
  return {
    base,
    base_branch: baseBranch,
    branch: row.wt.branch,
    slug: row.wt.slug,
    cwd: row.wt.path,
    pr: row.pr ? String(row.pr.number) : "",
    // The stage this worktree owns — the pinned `.sst/stage` (prefix-
    // guarded), else the slug-derived default. Any user shell action that
    // wants a stage handle (e.g. `sst remove --stage {{stage}}`) reads this.
    stage: expectedStage(row.wt),
    // Harness skill-invocation prefix (`/` for Claude Code, `$` for
    // OpenCode / Codex). Lets a prompt like `{{skill_prefix}}restack`
    // route to the right skill regardless of which harness receives it.
    // See `actionSkillPrefix` for how the target harness is picked.
    skill_prefix: skillPrefix,
  };
}

/**
 * Pick the harness whose skill-invocation prefix goes into `{{skill_prefix}}`
 * for this action launch.
 *
 *  - `target: "session"` prompts are injected into the row's live primary
 *    harness session, so the prefix must match that harness's skill syntax.
 *  - `kind: "shell"` actions run raw shell; if they reference
 *    `{{skill_prefix}}` at all it's to construct a skill call for the
 *    operator's current harness, so primary is the best guess.
 *  - Headless prompt actions (the default `target`) run the selected
 *    primary harness's non-interactive CLI (`claude -p`, `codex exec`,
 *    `opencode run`), so the prefix follows that harness too.
 *  - `def === null` is the "Custom prompt…" entry, which is also a
 *    headless prompt action.
 */
export function actionSkillPrefix(
  _def: ActionDef | null,
  primaryHarness: HarnessId,
): string {
  return getHarness(primaryHarness).skillPrefix;
}
