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
import { resolveIssueId } from "../core/issue-tracker.ts";
import { lockLabel, lockStatus } from "../core/locks.ts";
import { canEnterSessionDuringLock } from "../core/session-readiness.ts";
import { expectedStage } from "../core/stage-safety.ts";
import { StatusKind } from "../core/types.ts";
import { owesPostMergeVerification } from "../core/work-status.ts";
import type { SyncState } from "../core/worktree.ts";

import { armedFromPr } from "./badges.ts";
import {
  GROUP_ARCHIVED,
  GROUP_INBOX,
  type WorktreeRow,
} from "./hooks/useWorktreeRows.ts";
import { visualKey, type VisualItem } from "./hooks/useVisualItems.ts";

/**
 * Where the cursor goes when the row under it LEAVES that slot — a
 * destroy, a clean sweep, an archive, a move to another section.
 *
 * Following the row is wrong for all four, and it's what the plain
 * key-anchored cursor does: `d` and `c` archive the row before the
 * background remove starts, so the cursor rides it down to the archived
 * block at the bottom of the board and sits there for the seconds the
 * teardown takes; filing a row into another section drags the cursor out
 * of the section the user is working through. The cursor belongs to the
 * PLACE, not to the row — it holds the slot and takes whatever moves up
 * into it.
 *
 * `departing` is every visual key leaving in THIS action, so a sweep
 * can't land the cursor on the next row it is about to destroy. Rows
 * already archived are skipped for the same reason: they're mid-teardown
 * from an earlier one. Preference order is down within the group, up
 * within the group, then the same pair unconstrained for when the whole
 * section is going. Null means the cursor isn't on a departing row, or
 * nothing survives to point at — the caller leaves the selection alone
 * and `useVisualItems`' index fallback takes over.
 */
export function cursorSuccessor(
  items: readonly VisualItem[],
  cursorIndex: number,
  departing: ReadonlySet<string>,
): string | null {
  const anchor = cursorIndex >= 0 ? items[cursorIndex] : undefined;
  if (!anchor || !departing.has(visualKey(anchor))) return null;
  // "Group" only means something for a live worktree row; a departing
  // remote/PR/section item just takes the nearest survivor.
  const group =
    anchor.kind === "wt" && !anchor.row.archived
      ? anchor.row.section ?? GROUP_INBOX
      : null;
  const groupOf = (it: VisualItem): string | null =>
    it.kind === "wt" && !it.row.archived
      ? it.row.section ?? GROUP_INBOX
      : it.kind === "section"
        ? it.sectionKey
        : null;
  const survives = (it: VisualItem): boolean =>
    !departing.has(visualKey(it)) && !(it.kind === "wt" && it.row.archived);
  const scan = (dir: 1 | -1, sameGroup: boolean): string | null => {
    for (let i = cursorIndex + dir; i >= 0 && i < items.length; i += dir) {
      const it = items[i]!;
      if (!survives(it)) continue;
      // Groups are contiguous, so the first survivor outside this one
      // ends the constrained scan rather than being skipped over.
      if (sameGroup && groupOf(it) !== group) return null;
      return visualKey(it);
    }
    return null;
  };
  if (group !== null) {
    const inGroup = scan(1, true) ?? scan(-1, true);
    if (inGroup !== null) return inGroup;
  }
  return scan(1, false) ?? scan(-1, false);
}

const REVIEW_REQUESTS_GROUP = "\0review-requests";

/** The contiguous section represented by a visible list item. */
function visualSectionKey(item: VisualItem): string {
  if (item.kind === "section") return item.sectionKey;
  if (item.kind === "pr") return REVIEW_REQUESTS_GROUP;
  if (item.kind === "wt") {
    return item.row.archived
      ? GROUP_ARCHIVED
      : item.row.section ?? GROUP_INBOX;
  }
  if (item.archived) return GROUP_ARCHIVED;
  return "section" in item.entry
    ? item.entry.section ?? GROUP_INBOX
    : GROUP_INBOX;
}

/**
 * Find the first visible item in the next/previous section. Expanded
 * sections begin at their first row; folded sections are represented by
 * their selectable header, so the same rule lands on the title naturally.
 */
export function sectionJumpTarget(
  items: readonly VisualItem[],
  cursorIndex: number,
  direction: 1 | -1,
): string | null {
  const current = items[cursorIndex];
  if (!current) return null;
  const currentSection = visualSectionKey(current);

  if (direction === 1) {
    for (let i = cursorIndex + 1; i < items.length; i++) {
      const item = items[i]!;
      if (visualSectionKey(item) !== currentSection) return visualKey(item);
    }
    return null;
  }

  let previousIndex = cursorIndex - 1;
  while (
    previousIndex >= 0 &&
    visualSectionKey(items[previousIndex]!) === currentSection
  ) {
    previousIndex--;
  }
  if (previousIndex < 0) return null;
  const previousSection = visualSectionKey(items[previousIndex]!);
  while (
    previousIndex > 0 &&
    visualSectionKey(items[previousIndex - 1]!) === previousSection
  ) {
    previousIndex--;
  }
  return visualKey(items[previousIndex]!);
}

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
 * bindings (open-editor, archive, …) should always go through here.
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
 * by the section move/rename bindings (J/K/L) and the global Shift+A
 * automations pause. Excludes every other modifier, including Meta
 * (Cmd) and Hyper: the kitty keyboard protocol exposes Hyper as its
 * own flag, but a Hyper key synthesized through skhd/yabai layers
 * Cmd+Shift (± Ctrl/Option), so `hyper+a` can arrive as a plain
 * `{shift, meta}` combo — guarding Meta keeps it from leaking into
 * these single-letter actions.
 */
export function isShiftedLetter(
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
    k.shift &&
    !k.ctrl &&
    !k.option &&
    !k.super &&
    !k.hyper &&
    !k.meta
  );
}

/**
 * Match a non-letter key (F-keys, Tab, …) by `name` with every modifier
 * released. Same "no leaking modifier" guard as `isPlainLetter`, for
 * bindings whose `name` isn't a single letter.
 */
export function isBareKey(
  k: {
    name: string;
    shift: boolean;
    ctrl: boolean;
    option: boolean;
    super?: boolean;
    hyper?: boolean;
    meta: boolean;
  },
  name: string,
): boolean {
  return (
    k.name === name &&
    !k.shift &&
    !k.ctrl &&
    !k.option &&
    !k.super &&
    !k.hyper &&
    !k.meta
  );
}

/** Like `isBareKey`, but requires Shift and no other modifier. */
export function isBareShiftedKey(
  k: {
    name: string;
    shift: boolean;
    ctrl: boolean;
    option: boolean;
    super?: boolean;
    hyper?: boolean;
    meta: boolean;
  },
  name: string,
): boolean {
  return (
    k.name === name &&
    k.shift &&
    !k.ctrl &&
    !k.option &&
    !k.super &&
    !k.hyper &&
    !k.meta
  );
}

/**
 * Filter a key sequence down to printable text so single keypresses
 * and pasted blobs both append cleanly, while control chars (escape,
 * backspace, embedded newlines from multi-line pastes) drop out.
 */
export function printableText(sequence: string | undefined, unicode = false): string {
  if (!sequence) return "";
  // Escape-sequence keypresses (function/arrow/nav keys — F10 arrives as
  // "\x1b[21~", arrows as "\x1b[A") lead with ESC. Stripping control chars
  // alone would leak the printable tail ("[21~", "[A") into the text, so
  // bail on a leading ESC outright — real typed text and pastes never
  // start with one.
  if (sequence.charCodeAt(0) === 0x1b) return "";
  // Human titles accept Unicode; identifier inputs retain their ASCII policy.
  // Keep joiners/variation selectors used by emoji, but never terminal control
  // characters, directional formatting, unpaired surrogates, or line breaks.
  if (unicode) return sequence.replace(/[\p{Cc}\p{Cs}\p{Zl}\p{Zp}\p{Bidi_Control}]/gu, "");
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
  | { input: string; anyAuthor: boolean; attach: boolean; gh?: number; base?: string }
  | { error: string };

/**
 * Parse the TUI's `new:` prompt value: positional words (issue id +
 * optional pasted title, a branch, or a slug — multiple words join
 * into one input), plus optional `--any` / `--attach` / `--gh <n>` /
 * `--base <ref>`.
 * Mirrors `wt new` so muscle memory carries over. A `defaultBase` from
 * the `N` keybinding seeds the base; an explicit `--base` overrides.
 */
export function parseNewInput(raw: string, defaultBase?: string): NewInput {
  const tokens = raw.trim().split(/\s+/).filter(Boolean);
  const positionals: string[] = [];
  let anyAuthor = false;
  let attach = false;
  let gh: number | undefined;
  let base = defaultBase;
  for (let i = 0; i < tokens.length; i++) {
    const t = tokens[i]!;
    if (t === "--any") {
      anyAuthor = true;
    } else if (t === "--attach") {
      attach = true;
    } else if (t === "--gh") {
      const n = Number(tokens[++i]);
      if (!Number.isInteger(n) || n <= 0) return { error: "--gh requires an issue number" };
      gh = n;
    } else if (t === "--base") {
      const next = tokens[++i];
      if (!next) return { error: "--base requires a ref" };
      base = next;
    } else if (t.startsWith("--")) {
      return { error: `unknown flag: ${t}` };
    } else {
      positionals.push(t);
    }
  }
  if (positionals.length === 0) return { error: "missing input" };
  // Multiple words are one input — `ENG-1953 fix calendar` reads as
  // id + pasted title (parseInput slugifies the tail).
  return { input: positionals.join(" "), anyAuthor, attach, gh, base };
}

/**
 * A worktree is safe to clean when the branch is finished upstream. We
 * accept three signals — local "merged into main", local "[gone]" after
 * a fetch+prune, or the PR itself being merged. The PR check catches
 * squash-merged branches before the next `R` lands, which is by far the
 * most common case with GitHub's default merge style.
 */
/**
 * Is "merge when ready" currently armed on this row's PR?
 *
 * Two different GitHub features answer that, and only one of them sets
 * `autoMerge`. On a base branch with a merge queue, arming ENQUEUES —
 * `autoMergeRequest` stays null forever and the queue entry is the only
 * evidence. Reading `autoMerge` alone therefore reported a queued PR as
 * unarmed, which is wrong in both directions at once: the picker offered
 * to arm an already-queued PR (GitHub then rejects the duplicate
 * enqueue), and the disarm path refused with "auto-merge not enabled" on
 * the exact PRs that most need dequeuing.
 *
 * Shared by the picker's label and the flow's guard so the two can never
 * disagree about what the keystroke is about to do.
 */
export function mergeWhenReadyArmed(row: WorktreeRow | undefined): boolean {
  if (!row?.pr) return false;
  // Shared with the PR glyph's armed colour (`prStateBadge`), so the
  // badge and the keystroke's guard can never disagree about whether
  // this row is already armed.
  return armedFromPr(row.pr, row.mq);
}

export function isCleanCandidate(row: WorktreeRow): boolean {
  // Archived worktrees opted out of the automatic lifecycle — don't
  // sweep them even if their branch has merged since.
  if (row.archived) return false;
  if (row.status.kind === StatusKind.Busy) return false;
  return rowHasLanded(row);
}

/**
 * Has this branch finished upstream? The three signals `isCleanCandidate`
 * accepts, without its archived/busy policy — because "the branch
 * landed" and "wt may sweep this row" are different questions and two
 * callers need the first one alone.
 *
 * The dot and the rank use it to decide whether a post-merge
 * verification has come due (`owesPostMergeVerification`), and an
 * archived row still owes one: archiving opts out of the SWEEP, which
 * is the only thing it was ever about.
 */
export function rowHasLanded(row: WorktreeRow): boolean {
  if (row.status.kind === StatusKind.Merged) return true;
  if (row.status.kind === StatusKind.Gone) return true;
  return row.pr?.state === "MERGED";
}

/**
 * What a destroy of this row would DESTROY rather than merely remove.
 * Null means the checkout holds nothing a `-D` branch delete and a
 * directory removal wouldn't be able to reconstruct.
 *
 * `isCleanCandidate` is a claim about the BRANCH (it landed upstream);
 * this is a claim about the WORKING TREE, and the two are independent —
 * a merged branch accumulates new uncommitted work the moment anyone
 * opens a session in it again. Every destroy path must consult this,
 * because no backend enforces it for us: `git worktree remove` refuses a
 * dirty checkout only by luck of its own default, and `rift remove`
 * trashes one outright (see `core/backend/rift.ts`). A rift worktree is
 * an independent clone, so its removal takes the objects, the branch and
 * the reflog with it — there is no dangling-object recovery.
 *
 * "Still loading" is a hazard, not an absence of one: both fields read
 * `undefined` while their queries load or after an error, and treating
 * that window as clean is how a sweep deletes unsaved work.
 */
export type DestroyHazard =
  | { kind: "unknown" }
  | { kind: "dirty"; count: number }
  | { kind: "unpushed"; count: number }
  /**
   * The odd one out, and deliberately here rather than in a guard of
   * its own: nothing in the checkout is unrecoverable, but the
   * OBLIGATION is. A landed branch owing a deployed-environment check
   * (`--verify-after-merge`) is exactly the row a sweep would take,
   * and taking it deletes the context the check needs along with any
   * trace that it was owed — after which nothing anywhere says the
   * verification never happened. Listing it here is what makes every
   * destroy path inherit the guard, since sweeps never force.
   */
  | { kind: "unverified"; steps: string };

/**
 * Commits that exist ONLY in this checkout. `sync.remote` counts against
 * `origin/<branch>`, so when it's present the answer is exact.
 *
 * When there's no such ref the branch is either unpushed (everything
 * since the base would be lost) or squash-merged and pruned (those same
 * commits are already on trunk under a different sha). `wt rm` draws
 * exactly this distinction with its `landed` short-circuit; without it,
 * every squash-merged row reads as unpushed and the `c` sweep keeps a
 * board full of rows it was built to clear.
 */
function localOnlyCommits(row: WorktreeRow, sync: SyncState): number {
  if (sync.remote) return sync.remote.ahead;
  return isCleanCandidate(row) ? 0 : sync.main.ahead;
}

/**
 * Every hazard the row carries, worst first. `destroyHazard` takes the
 * first for a refusal message; the `d` confirm lists them all, since
 * "1 uncommitted file will be lost" understates a row that also holds
 * unpushed commits.
 */
export function destroyHazards(row: WorktreeRow): DestroyHazard[] {
  const sync = row.fields.sync.data;
  if (row.fields.dirty.data === undefined || sync === undefined) {
    return [{ kind: "unknown" }];
  }
  const out: DestroyHazard[] = [];
  const dirty = row.fields.dirty.data.length;
  if (dirty > 0) out.push({ kind: "dirty", count: dirty });
  const unpushed = localOnlyCommits(row, sync);
  if (unpushed > 0) out.push({ kind: "unpushed", count: unpushed });
  // Last: real data loss outranks an outstanding obligation when a
  // confirm has room for one line.
  if (owesPostMergeVerification(row.work, rowHasLanded(row))) {
    out.push({ kind: "unverified", steps: row.work!.verifyAfterMerge! });
  }
  return out;
}

export function destroyHazard(row: WorktreeRow): DestroyHazard | null {
  return destroyHazards(row)[0] ?? null;
}

/** Human phrasing for a hazard, shared by every refusal message. */
export function destroyHazardLabel(hazard: DestroyHazard): string {
  switch (hazard.kind) {
    case "unknown":
      return "dirty/unpushed state still loading";
    case "dirty":
      return `${hazard.count} uncommitted change${hazard.count === 1 ? "" : "s"}`;
    case "unpushed":
      return `${hazard.count} unpushed commit${hazard.count === 1 ? "" : "s"}`;
    case "unverified":
      // Deliberately without the steps. `verifyAfterMerge` is the one
      // field with no length budget — an executable contract for an
      // agent that is not the one who wrote it — and every consumer of
      // this label is a SCAN LINE: a `c` modal row, a `d` confirm that
      // joins reasons with commas and appends "will be lost", a toast.
      // Fourteen numbered steps in any of those buries the other
      // hazards next to it. The details pane is where the text lives.
      return "post-merge verification still owed";
  }
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
    // The tracker id in the slug (`coz-2176-…` → `COZ-2176`), empty when
    // the slug carries none. Derivable from `{{slug}}` by any shell
    // action, and supplied here anyway: a config re-implementing
    // `branch.id_pattern` in sed is a second definition of the same
    // convention, and the two drift the first time the pattern changes.
    issue_id: resolveIssueId(row.wt.slug, row.issueId) ?? "",
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
 *  - `target: "session"` prompts are sent to the row's live primary
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
