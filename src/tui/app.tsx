import { useEffect, useMemo, useRef, useState } from "react";
import { useIsFetching, useQuery } from "@tanstack/react-query";
import { useKeyboard, useRenderer, useTerminalDimensions } from "@opentui/react";

import {
  actionRegistry,
  evaluateActionRequirements,
  type ActionDef,
  type ActionRun,
  type ActionVars,
} from "../core/actions.ts";
import { config, configFilePath } from "../core/config.ts";
import {
  createWorktree,
  parseInput,
  spawnBackgroundRemove,
} from "../core/lifecycle.ts";
import {
  editReviewers,
  markPullRequestReady,
} from "../core/github.ts";
import { armMergeWhenReady } from "../core/graphite.ts";
import {
  addClaudeName,
  buildClaudeSessionEntries,
  nameInUse,
  nextAutoName,
  removeClaudeName,
  validateSessionName,
} from "../core/claude-sessions.ts";
import { graphiteUrlFromGithubPr } from "../core/graphite.ts";
import { linearUrlForSlug } from "../core/linear.ts";
import { lockLabel, lockStatus } from "../core/locks.ts";
import { createLogger } from "../core/logger.ts";
import {
  type LiveSessionDesc,
  sessionTailRegistry,
} from "../core/session-tail.ts";
import { removeShellLog, shellTailRegistry } from "../core/shell-tail.ts";
import { stageUrl } from "../core/stage.ts";
import {
  claudeSessionName,
  diffCommandUsesBase,
  killAllSessionsFor,
  killClaudeNamedSession,
  killDiffSession,
  killSession,
  killShellSession,
  type SessionKind,
  WT_SOURCE_SLUG,
} from "../core/tmux.ts";
import { StatusKind } from "../core/types.ts";
import { claudeUsageQuery, patchPullRequest, useWtActions, type GithubData } from "../state/index.ts";
import type { ClaudeUsage } from "../core/claude-usage.ts";

import {
  ActionEditModal,
  ActionPickerModal,
  type ActionPickerState,
  type PickerItem,
} from "./panels/action-picker.tsx";
import { CleanConfirmModal } from "./panels/clean-confirm.tsx";
import { Details } from "./panels/details.tsx";
import { Footer, type FooterMode } from "./panels/footer.tsx";
import { HelpOverlay } from "./panels/help.tsx";
import { KillActionConfirmModal } from "./panels/kill-action-confirm.tsx";
import { KillSessionConfirmModal } from "./panels/kill-session-confirm.tsx";
import { MultiPickerModal, PickerModal, type MultiPickerItem } from "./panels/picker.tsx";
import { OutputsPicker } from "./panels/outputs-picker.tsx";
import { OutputViewer } from "./panels/output-viewer.tsx";
import { previewFocusPatch, togglePinPatch } from "./picker-preview.ts";
import {
  SectionPickerModal,
  type SectionPickerItem,
} from "./panels/section-picker.tsx";
import {
  SessionsPickerList,
  SessionsPickerNew,
} from "./panels/sessions-picker.tsx";
import { WorktreeList } from "./panels/list.tsx";
import { YankModal, yankItemsFor } from "./panels/yank.tsx";
import { enterClaudeSession } from "./claude-session.ts";
import { enterDiffSession } from "./diff-session.ts";
import { enterShellSession } from "./shell-session.ts";
import { useAction, useActionVisible, useActiveActions } from "./hooks/useAction.ts";
import {
  useActiveDiffSessions,
  useActiveShellSessions,
  useClaudeSessionsBySlug,
} from "./hooks/useActiveSessions.ts";
import { useOutputs } from "./hooks/useOutputs.ts";
import {
  type Output,
  actionOutputId,
  destroyOutputId,
  eventsOutputId,
  indexOfOutput,
  outputsForSlug,
  sessionOutputId,
} from "../core/outputs.ts";
import { useAutoCopy } from "./hooks/useAutoCopy.ts";
import { useLogTails } from "./hooks/useLogTails.ts";
import { usePaste } from "./hooks/usePaste.ts";
import { useTerminalFocus } from "./hooks/useTerminalFocus.ts";
import { useWorktreeRows, type WorktreeRow } from "./hooks/useWorktreeRows.ts";
import { hideFrontmostAlacritty, openInZed, openUrl, writeClipboard, WT_REPO_PATH } from "./helpers.ts";
import { theme } from "./theme.ts";

/**
 * Resolve the diff base ref for a worktree row. Same priority chain
 * as `useWorktreeRows.resolveStackedOn` exposes: a parent branch when
 * the row is stack-detected or its PR targets a non-trunk base,
 * otherwise `origin/<config.branch.base>`. Used by the F11 handler to
 * fill `{{base}}` in `[diff].command` and by the kill-on-base-change
 * effect to detect when a stacked row's parent moved.
 */
function resolveDiffBase(row: WorktreeRow): string {
  return row.stackedOn?.diffBase ?? `origin/${config.branch.base}`;
}

/** Per-worktree pin/focus state for the Outputs system. */
type SlugFocus = { focused: string | null; pinned: string | null };
/** Bucket key for the "no row selected" state. Slugs are user-
 *  generated branch names with limited charset; this sentinel can't
 *  collide. */
const NO_ROW_KEY = "__no_row__";
const EMPTY_FOCUS: SlugFocus = { focused: null, pinned: null };

const appLog = createLogger("[app]");
const newLog = createLogger("[new]");
const configLog = createLogger("config");
const wtLog = createLogger("wt");

export type TuiExit = { kind: "quit" };

type Props = {
  onExit: (e: TuiExit) => void;
};

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
function isPlainLetter(
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
function isShiftedLetter(
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
function printableText(sequence: string | undefined): string {
  if (!sequence) return "";
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
 * filter / new-worktree / rename inputs still use `printableText`.
 */
function printableMultiline(sequence: string | undefined): string {
  if (!sequence) return "";
  let out = "";
  for (let i = 0; i < sequence.length; i++) {
    const ch = sequence[i]!;
    if (ch === "\n" || ch === "\t" || (ch >= " " && ch <= "~")) out += ch;
  }
  return out;
}

type NewInput =
  | { input: string; anyAuthor: boolean; base?: string }
  | { error: string };

/**
 * Parse the TUI's `new:` prompt value: one positional arg
 * (linear-id | branch | slug), plus optional `--any` / `--base <ref>`.
 * Mirrors `wt new` so muscle memory carries over. A `defaultBase` from
 * the `N` keybinding seeds the base; an explicit `--base` overrides.
 */
function parseNewInput(raw: string, defaultBase?: string): NewInput {
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
 * Every overlay/modal the TUI can display. Exactly one is active at
 * a time (or `null`); the keyboard handler dispatches by `kind` and
 * the JSX renders by `kind`. New overlays should add a variant here
 * rather than a parallel `useState<boolean>`.
 *
 * `branchPicker` suspends a caller (`doNew`'s `parseInput` awaits the
 * resolver). Everything else is fire-and-forget: opening sets state,
 * the user picks/cancels, the handler fires the side-effect and
 * clears `modal`.
 */
type Modal =
  | { kind: "help" }
  | { kind: "cleanConfirm" }
  | { kind: "yank" }
  | {
      kind: "branchPicker";
      title: string;
      items: string[];
      index: number;
      resolve: (picked: string | null) => void;
    }
  | {
      kind: "reviewerPicker";
      title: string;
      items: MultiPickerItem[];
      index: number;
      checked: Set<string>;
      /** Snapshot of currently-requested logins; diffed on submit. */
      original: Set<string>;
      slug: string;
      prNumber: number;
    }
  | {
      kind: "sectionPicker";
      title: string;
      slug: string;
      items: SectionPickerItem[];
      index: number;
      /**
       * When non-null, the modal is in "+ new section" name-input mode.
       * Typed characters append here instead of navigating the list.
       */
      newName: string | null;
    }
  | { kind: "actionPicker"; state: ActionPickerState }
  | { kind: "outputsPicker"; index: number }
  | {
      kind: "claudeSessionsPicker";
      slug: string;
      /** Index into the list view (live sessions + a trailing "+ new"). */
      index: number;
    }
  | {
      kind: "claudeSessionsNew";
      slug: string;
      input: string;
      error: string | null;
    }
  | { kind: "killActionConfirm"; slug: string; actionName: string }
  | {
      kind: "killSessionConfirm";
      slug: string;
      // Claude kill is reachable only via the picker (`x`), which
      // bypasses this modal entirely; only diff and shell still use
      // the y/N confirm.
      sessionKind: "diff" | "shell";
    };

/**
 * A worktree is safe to clean when the branch is finished upstream. We
 * accept three signals — local "merged into main", local "[gone]" after
 * a fetch+prune, or the PR itself being merged. The PR check catches
 * squash-merged branches before the next `R` lands, which is by far the
 * most common case with GitHub's default merge style.
 */
function isCleanCandidate(row: WorktreeRow): boolean {
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
 * Variables exposed to action templates as `{{name}}`. Kept here (not
 * in `core/actions.ts`) because it depends on `WorktreeRow`, which is
 * a TUI-layer type — the registry stays UI-agnostic.
 *
 * `base` mirrors the details-pane base value (may be a SHA when the
 * stack signal is `patch-id`); `base_branch` is always a named ref —
 * the right thing to plug into `git rebase` or a "rebase on X" prompt.
 */
function buildActionVars(row: WorktreeRow): ActionVars {
  const baseBranch = row.stackedOn?.branch ?? config.branch.base;
  const base = row.stackedOn?.diffBase ?? config.branch.base;
  return {
    base,
    base_branch: baseBranch,
    branch: row.wt.branch,
    slug: row.wt.slug,
    cwd: row.wt.path,
    pr: row.pr ? String(row.pr.number) : "",
  };
}

/**
 * Right-aligned title-bar slot that surfaces the Claude Code
 * statusline's API utilization snapshot. Data comes from the
 * statusline's own cache file; we never call Anthropic directly. Two
 * clusters — 5h window and rolling 7d window — each pairing the
 * percentage with the time remaining until that window resets. The
 * 30-second ticker keeps the remaining-time labels drifting forward
 * between cache writes (the underlying query refetches once a minute).
 */
const USAGE_STALE_MS = 30 * 60 * 1000;

function ClaudeUsageBadge() {
  const { data } = useQuery(claudeUsageQuery());
  const [nowMs, setNowMs] = useState(() => Date.now());
  useEffect(() => {
    const id = setInterval(() => setNowMs(Date.now()), 30_000);
    return () => clearInterval(id);
  }, []);
  const formatted = useMemo(() => formatClaudeUsage(data, nowMs), [data, nowMs]);
  if (!formatted) return null;
  const { fiveHour: five, sevenDay: seven } = formatted;
  return (
    <box flexShrink={0} flexDirection="row">
      <text fg={theme.fgDim}>{"  🔥 "}</text>
      <text fg={pctColor(five.pct)}>{`5h ${five.pct}%`}</text>
      {five.remaining ? <text fg={theme.fgDim}>{` (${five.remaining})`}</text> : null}
      <text fg={theme.fgDim}>{"  ·  "}</text>
      <text fg={pctColor(seven.pct)}>{`7d ${seven.pct}%`}</text>
      {seven.remaining ? <text fg={theme.fgDim}>{` (${seven.remaining})`}</text> : null}
    </box>
  );
}

/**
 * Match statusline.sh's coloring: cool/dim under 60%, warm 60-80%,
 * hot at 80%+. Only the percentage itself is tinted — the surrounding
 * "5h ..." / "(time)" framing stays dim so the colored numbers pop.
 */
function pctColor(pct: number): string {
  if (pct >= 80) return theme.err;
  if (pct >= 60) return theme.warn;
  return theme.fg;
}

type FormattedUsage = {
  fiveHour: { pct: number; remaining: string | null };
  sevenDay: { pct: number; remaining: string | null };
};

function formatClaudeUsage(
  usage: ClaudeUsage | null | undefined,
  nowMs: number,
): FormattedUsage | null {
  if (!usage) return null;
  // Mirror statusline.sh's CACHE_STALE_AGE: don't display data older
  // than 30 minutes — at that point the user has either exited Claude
  // Code or it's failing to refresh, and a 6h-stale "5h 12%" is worse
  // than nothing.
  if (nowMs - usage.cachedAtMs > USAGE_STALE_MS) return null;
  return {
    fiveHour: {
      pct: Math.round(usage.fiveHour.utilization),
      remaining: formatRemaining(usage.fiveHour.resetsAt, nowMs),
    },
    sevenDay: {
      pct: Math.round(usage.sevenDay.utilization),
      remaining: formatRemaining(usage.sevenDay.resetsAt, nowMs),
    },
  };
}

/**
 * Format a duration as the two coarsest non-zero units. Picks d+h when
 * the duration spans days, h+m otherwise. Drops the smaller unit when
 * it would render as 0 — `2h0m` becomes `2h`. Returns null on missing
 * or unparseable input.
 */
function formatRemaining(iso: string | null, nowMs: number): string | null {
  if (!iso) return null;
  const target = Date.parse(iso);
  if (Number.isNaN(target)) return null;
  const ms = Math.max(0, target - nowMs);
  const d = Math.floor(ms / 86_400_000);
  const h = Math.floor((ms % 86_400_000) / 3_600_000);
  const m = Math.floor((ms % 3_600_000) / 60_000);
  if (d > 0) return h > 0 ? `${d}d${h}h` : `${d}d`;
  if (h > 0) return m > 0 ? `${h}h${m}m` : `${h}h`;
  return `${m}m`;
}

export function App({ onExit }: Props) {
  const { width, height } = useTerminalDimensions();
  const renderer = useRenderer();
  const { rows, isLoading } = useWorktreeRows();
  const {
    refreshAll,
    refreshStale,
    refreshGithub,
    refreshGraphite,
    refreshTmuxSessions,
    optimisticRemoveClaude,
    fetchContributors,
    fetchMe,
    clearAll,
    invalidateWorktree,
    refreshAiSummary,
    toggleArchived,
    archive,
    setSection,
    swapOrder,
    placeSlug,
    renameSection,
    moveSection,
    mutate,
  } = useWtActions();
  // Cursor is tracked by slug, not index. Slug identity survives row
  // moves (archive, section change, manual reorder) without any
  // explicit "follow this row" plumbing — the visual index falls out
  // of `filteredRows.findIndex(r.slug === sel)` on each render. When
  // the selected slug disappears (destroy), `lastIndexRef` snaps the
  // cursor to the row that took its place rather than jumping to the
  // top of the list.
  const [sel, setSel] = useState<string | null>(null);
  const lastIndexRef = useRef(0);
  const [footer, setFooter] = useState<FooterMode>({ kind: "legend" });
  // All modal/overlay state collapsed into one discriminated union so
  // the "only one modal is open at a time" invariant is structural
  // rather than emergent. Per-modal payload (cursor index, picker
  // items, slug context) lives on its variant. The keyboard handler
  // and JSX both `switch` on `modal.kind`.
  const [modal, setModal] = useState<Modal | null>(null);
  const [filter, setFilter] = useState("");
  // Last section the user moved a row into. Used to default the
  // section-picker cursor — the common case is "moving several
  // adjacent worktrees into the same section", and re-aiming on
  // every open eats keystrokes. Reset to `null` on rename so the
  // sticky target doesn't dangle.
  const [lastMoveTarget, setLastMoveTarget] = useState<string | null>(null);
  // Section the user is renaming, if any. Sits alongside the footer
  // input mode (footer carries the prompt + value, this carries the
  // identity of the thing being renamed). Not folded into `modal`
  // because the rename UX uses the footer, not an overlay.
  const [pendingRename, setPendingRename] = useState<string | null>(null);
  // Per-worktree bottom-pane state. Each slug has its own focused
  // (explicit pick from picker / cycle keys) and pinned (sticky
  // override) output. Switching rows restores that worktree's last
  // selection — so monitoring an action on one slug and a CC session
  // on another stays mutually independent. The `__no_row__` bucket
  // covers the "nothing selected" edge (filter excludes everything,
  // brand-new repo) and pins down what the global pane shows there.
  const [slugFocus, setSlugFocus] = useState<Record<string, SlugFocus>>({});
  const toastTimer = useRef<Timer | null>(null);

  // Auto-tail every busy worktree so logs surface in the activity pane
  // without user intervention. Returns the active set so rows can flag
  // a visual "is tailing" hint.
  const activeTails = useLogTails(rows);

  // Mouse-select anywhere → auto-copy on release.
  useAutoCopy();

  // Refocusing the terminal window refetches any observed query that
  // has crossed its staleTime — cheap and idempotent. Fresh data stays
  // put; there's no `git fetch origin` or full invalidation (that's
  // still `r`). Matches how the rest of the TUI treats user input:
  // "looking at it" counts as engagement that can freshen stale data.
  useTerminalFocus(() => {
    refreshStale();
  });

  // Bracketed paste → append into whichever text mode is active. No-op
  // in legend/toast/confirm modes since paste only makes sense when the
  // user is typing.
  usePaste((text) => {
    if (modal?.kind === "actionPicker" && modal.state.mode === "edit") {
      const clean = printableMultiline(text);
      if (!clean) return;
      setModal({
        ...modal,
        state: { ...modal.state, extras: modal.state.extras + clean },
      });
      return;
    }
    const clean = printableText(text);
    if (!clean) return;
    if (footer.kind === "filter") {
      const next = footer.value + clean;
      setFooter({ kind: "filter", value: next });
      setFilter(next);
      setSel(null);
    } else if (footer.kind === "input") {
      setFooter({ ...footer, value: footer.value + clean });
    }
  });

  const filteredRows = useMemo(() => {
    const needle = filter.trim().toLowerCase();
    if (!needle) return rows;
    return rows.filter((r) => r.wt.slug.toLowerCase().includes(needle));
  }, [rows, filter]);

  const cleanCandidates = useMemo(
    () => rows.filter((r) => isCleanCandidate(r)),
    [rows],
  );

  // Resolve the selected slug to a visual index. When the slug isn't
  // in the current filtered set (destroyed, filtered out, never set),
  // fall back to the last known visual index, clamped to the new
  // length. That fallback is what makes "destroy the selected row"
  // land the cursor on the row that took its place.
  const lookupIndex =
    sel === null ? -1 : filteredRows.findIndex((r) => r.wt.slug === sel);
  const cursorIndex =
    filteredRows.length === 0
      ? -1
      : lookupIndex >= 0
        ? lookupIndex
        : Math.min(lastIndexRef.current, filteredRows.length - 1);
  const current = cursorIndex >= 0 ? filteredRows[cursorIndex] : undefined;
  // Stash the resolved index so it's available as a fallback the next
  // time the slug can't be found. Writing during render is safe — the
  // value is derived purely from this render's inputs, the write is
  // idempotent, and reading it elsewhere happens in the same render.
  if (cursorIndex >= 0 && cursorIndex !== lastIndexRef.current) {
    lastIndexRef.current = cursorIndex;
  }

  const listWidth = Math.max(32, Math.min(52, Math.floor(width * 0.44)));
  // Middle (list + details) is capped; activity absorbs the rest. Title
  // and footer take 1 row each, so `height - 2` is the usable column
  // budget split between middle and activity.
  const middleMax = 20;
  const activityHeight = Math.max(7, height - 2 - middleMax);

  // Action runtime state for the *selected* worktree. `currentRun`
  // drives the activity-pane swap (showing the streamed claude output
  // in place of events) and the `!`-key dispatch (open kill-confirm
  // when running, open picker otherwise).
  const currentSlug = current?.wt.slug;
  const currentRun = useAction(currentSlug);
  const showActionViewer = useActionVisible(currentSlug);
  // Set of slugs whose action is in flight RIGHT NOW (no recent-window
  // tail). Drives the leftmost cluster glyph in `WorktreeList` so the
  // user has at-a-glance awareness of what's running on rows they're
  // not currently viewing.
  const activeActions = useActiveActions();
  // Per-slug list of live claude session names (`null` = primary).
  // Drives the tail-registry reconcile, the sessions picker, the
  // count badge in the row list, and the auto-output focus rule.
  const claudeSessionsBySlug = useClaudeSessionsBySlug();
  // Pre-compute the sessions-picker entries for the current modal
  // slug (when the picker is open). Memoized so we don't pay the
  // `listClaudeNames` disk read on every render — only when the live
  // session set or the modal target changes. Empty array when the
  // modal is closed or pointed at a different kind.
  const pickerEntries = useMemo(() => {
    if (modal?.kind !== "claudeSessionsPicker") return [];
    return buildClaudeSessionEntries(
      modal.slug,
      claudeSessionsBySlug.get(modal.slug) ?? [],
    );
  }, [modal, claudeSessionsBySlug]);
  // Parallel set for diff sessions — used by the Shift+F11 hint so
  // the kill-confirm only opens when there's something to kill.
  const activeDiffSessions = useActiveDiffSessions();
  // Same for shell sessions, gating Shift+F10.
  const activeShellSessions = useActiveShellSessions();

  // Reconcile session tailers against the live (slug, name) set so the
  // jsonl-watch lifecycle tracks the daemon. Re-runs whenever the live
  // set changes; the registry is otherwise idempotent so this is safe
  // to call on every render-driven change. Path comes from `rows` so
  // we always seed against the worktree's actual cwd (the wtPath the
  // tmux session was created with).
  useEffect(() => {
    const pathBySlug = new Map<string, string>();
    for (const r of rows) pathBySlug.set(r.wt.slug, r.wt.path);
    const live: LiveSessionDesc[] = [];
    for (const [slug, names] of claudeSessionsBySlug) {
      const wtPath = pathBySlug.get(slug);
      if (!wtPath) continue;
      for (const name of names) live.push({ slug, name, wtPath });
    }
    sessionTailRegistry.reconcile(live);
  }, [rows, claudeSessionsBySlug]);

  // Same shape for the F10 shell tail: spin a tailer per live shell
  // session, drop tailers for sessions that ended. The shell registry
  // doesn't need the worktree path — its log file is keyed on the
  // bare slug under wt's cache dir.
  useEffect(() => {
    const live = new Set<string>();
    for (const r of rows) {
      if (activeShellSessions.has(r.wt.slug)) live.add(r.wt.slug);
    }
    shellTailRegistry.reconcile(live);
  }, [rows, activeShellSessions]);

  // Kill any live `<slug>-diff` tmux session whose resolved base ref
  // has changed since the session was opened, so the next F11 spawns
  // fresh against the new ref instead of leaving the user staring at a
  // diff vs the prior parent. Triggered by stack re-detection (reflog
  // says we rebased onto a different worktree) or PR base flips. Only
  // runs when the user's diff command actually depends on `{{base}}` —
  // commands like `gitu` ignore the base and shouldn't be torn down on
  // unrelated re-resolutions.
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
      log.event.info(`diff base changed (${prev} → ${next}); killing diff session`);
      void (async () => {
        await killDiffSession(slug);
        await refreshTmuxSessions();
      })();
    }
    // Drop entries for slugs that no longer exist so the map doesn't
    // grow unboundedly across the session.
    for (const slug of [...lastDiffBase.current.keys()]) {
      if (!seen.has(slug)) lastDiffBase.current.delete(slug);
    }
  }, [rows, activeDiffSessions, refreshTmuxSessions]);

  // Custom action effect dispatch — see rule (3) in the architecture
  // block at the top of `state/hooks.ts`. Each action carries an
  // `affects` tag set captured at start time; on every transition
  // from `running` → terminal status, fan that out to the matching
  // invalidation helpers. The `handled` set keys on `slug@endedAt`
  // so a completion fires exactly once even when the registry
  // notifies for unrelated state churn afterwards.
  //
  // `handled` and the helper closures live in refs so the effect
  // subscribes exactly once at mount. `useWtActions` returns a fresh
  // object every render — without the ref indirection the deps array
  // would tear down + re-seed on every render, and a completion that
  // fires inside that window can be lost to the seed before dispatch
  // runs.
  const actionHelpersRef = useRef({ invalidateWorktree, refreshGithub });
  actionHelpersRef.current = { invalidateWorktree, refreshGithub };
  const actionHandledRef = useRef<Set<string>>(new Set());
  useEffect(() => {
    const handled = actionHandledRef.current;
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
        const { invalidateWorktree: inv, refreshGithub: rg } = actionHelpersRef.current;
        for (const tag of run.affects) {
          switch (tag) {
            case "git":
              void inv(run.slug);
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
      }
    });
  }, []);

  // Slugs whose lock op is `"remove"` — drives the destroy outputs
  // surfaced in the picker. Computed from `rows` (each busy row
  // exposes the lock's `op`) so it tracks the same source the
  // worktree list uses for its own busy state, no extra query.
  const destroyingSlugs = useMemo(
    () =>
      rows
        .filter(
          (r) => r.status.kind === StatusKind.Busy && r.status.op === "remove",
        )
        .map((r) => r.wt.slug),
    [rows],
  );
  // Global, slug-tagged list of everything renderable in the bottom
  // pane. Filtered per worktree at the consumption site — see
  // `visibleOutputs` below.
  const outputs = useOutputs({ destroyingSlugs });
  // Bucket key for the current worktree's pin/focus state. Stays in
  // sync with the selected row; falls back to `NO_ROW_KEY` when
  // nothing is selected so the picker / pin still have a place to
  // store state.
  const focusKey = currentSlug ?? NO_ROW_KEY;
  const focusBucket = slugFocus[focusKey] ?? EMPTY_FOCUS;
  // Outputs visible while sitting on this worktree: the global ones
  // (events) plus this worktree's actions and sessions. Picker, cycle
  // keys, and the displayed-output resolver all see the same filtered
  // universe — that's what makes the per-worktree experience
  // coherent.
  const visibleOutputs = useMemo(
    () => outputsForSlug(outputs, currentSlug ?? null),
    [outputs, currentSlug],
  );
  // Auto-rule for the bottom pane when the user hasn't explicitly
  // picked anything for this worktree: prefer the selected row's
  // in-flight destroy, then its running/recent action, then a live
  // claude tmux session, then events. Destroy beats action because
  // when a worktree is being torn down the user almost certainly
  // wants to watch progress (and an action can't run during destroy
  // — the lock is exclusive — so the precedence isn't really
  // contested). Depends on `currentRun?.startedAt` (not the whole
  // run object) so a stream of line appends doesn't re-run the memo
  // for an unchanged id.
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
      // Multi-session: prefer primary when live, otherwise the
      // most-recently-active named session — same sort `;` and the
      // outputs picker use, so the auto-pick lines up with what the
      // user sees up top in `:`. Only ids that exist in
      // `visibleOutputs` win the auto-rule (the lookup at
      // `displayedOutput` enforces that), so refusing to invent an
      // id for a dead session keeps the events fallback reachable.
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
    visibleOutputs,
  ]);
  // Pin > explicit user pick > auto, all scoped to the current
  // worktree's bucket. If the chosen id has evicted from the visible
  // list (action FIFO'd, session ended, or this slug's id belongs to
  // a different slug after a row change) drop back through the
  // precedence chain.
  const desiredOutputId =
    focusBucket.pinned ?? focusBucket.focused ?? autoOutputId;
  const displayedOutput: Output =
    visibleOutputs.find((o) => o.id === desiredOutputId) ??
    visibleOutputs.find((o) => o.id === autoOutputId) ??
    visibleOutputs[0]!; // events always present in the filtered list
  // GC stale per-slug state: drop bucket fields whose target output
  // is no longer in `outputs`, and drop entire buckets for slugs
  // that are no longer worktrees. Without the second sweep, a long
  // wt session destroying and recreating worktrees would accumulate
  // dead entries forever. Surface a dim event line whenever a
  // non-empty bucket evicts so a user who pinned an action and then
  // destroyed the worktree gets a breadcrumb instead of a silent
  // disappearance.
  //
  // Implementation note: the diff (which buckets to drop, which to
  // log about) is computed in a non-updater pass first, then the
  // event log emit + state update fire outside `setSlugFocus`.
  // Emitting `appLog.event.dim` from inside an updater would mutate
  // the events store, which feeds back into this effect via
  // `outputs` → `useOutputs` and trip the same effect on the next
  // render. Cleaner to keep the side effect out of the updater.
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
        if (bucket.focused !== null || bucket.pinned !== null) {
          evictedSlugs.push(key);
        }
        changed = true;
        continue;
      }
      const focused =
        bucket.focused && liveOutputIds.has(bucket.focused)
          ? bucket.focused
          : null;
      const pinned =
        bucket.pinned && liveOutputIds.has(bucket.pinned)
          ? bucket.pinned
          : null;
      if (focused !== bucket.focused || pinned !== bucket.pinned) {
        changed = true;
      }
      if (focused === null && pinned === null) {
        // Drop empty buckets so the map doesn't grow unbounded
        // through ordinary navigation. `setFocus` already prevents
        // writing all-null buckets, but defending the invariant
        // here means the GC sweep is correct even if some other
        // code path ever inserts one.
        changed = true;
        continue;
      }
      next[key] = { focused, pinned };
    }

    if (!changed) return;
    for (const key of evictedSlugs) {
      appLog.event.dim(`dropped output state for ${key} (worktree gone)`);
    }
    setSlugFocus(next);
  }, [outputs, rows, slugFocus]);

  // Helper — applies a partial update to the current worktree's
  // bucket without forcing every callsite to spell out the spread.
  function setFocus(slug: string | null, patch: Partial<SlugFocus>): void {
    const key = slug ?? NO_ROW_KEY;
    setSlugFocus((prev) => {
      const cur = prev[key] ?? EMPTY_FOCUS;
      const next = { ...cur, ...patch };
      // Drop the bucket entirely when both fields collapse to null
      // (parity with the GC effect; keeps the map tight).
      if (next.focused === null && next.pinned === null) {
        if (!(key in prev)) return prev;
        const { [key]: _, ...rest } = prev;
        return rest;
      }
      return { ...prev, [key]: next };
    });
  }

  function toast(message: string, color = theme.ok, ms = 2500): void {
    if (toastTimer.current) clearTimeout(toastTimer.current);
    setFooter({ kind: "toast", message, color });
    toastTimer.current = setTimeout(() => {
      setFooter((f) => (f.kind === "toast" ? { kind: "legend" } : f));
      toastTimer.current = null;
    }, ms);
  }

  function quit(): void {
    onExit({ kind: "quit" });
  }

  /**
   * Standard error reporter for state-mutation chains. Disk writes
   * inside `wtstate.ts` can throw on EACCES / ENOSPC; we surface as
   * an event log line + toast.
   */
  function reportActionError(label: string, err: unknown): void {
    const msg = err instanceof Error ? err.message : String(err);
    appLog.event.err(`${label} failed: ${msg}`);
    toast(`${label} failed: ${msg}`, theme.err, 3000);
  }

  /**
   * Build the section-picker item list. Excludes the current row's
   * section since "move this row to where it already is" is never
   * useful (the user explicitly asked to drop that as clutter).
   * "+ new section" sits at the bottom with `l` as its quick chord
   * trigger so `l l` creates a fresh section in two keystrokes.
   */
  function buildSectionItems(currentSection: string | null): SectionPickerItem[] {
    const items: SectionPickerItem[] = [];
    if (currentSection !== null) items.push({ kind: "none" });
    const seen = new Set<string>();
    for (const r of rows) {
      if (r.archived) continue;
      if (r.section === null || seen.has(r.section)) continue;
      seen.add(r.section);
      if (r.section === currentSection) continue;
      items.push({ kind: "section", name: r.section });
    }
    items.push({ kind: "create" });
    return items;
  }

  /**
   * Unified Shift+J/K. Walks the active-row list one step in `dir`:
   *   - Same-section neighbor → swap orders (within-section reorder).
   *   - Cross-section neighbor → move row to the adjacent edge of the
   *     target section (top of next on `J`, bottom of prev on `K`),
   *     so the row visually slides one position the way the user
   *     expects rather than leaping to the far end of the target.
   * The archive boundary is hard: rows can't cross into archived via
   * J/K — that's `a`'s job.
   */
  function doShiftMove(dir: -1 | 1): void {
    if (!current) return;
    if (current.archived) {
      toast("archived rows don't reorder, use `a` to restore", theme.fgDim, 1500);
      return;
    }
    if (filter) {
      toast("clear filter to reorder", theme.warn, 1500);
      return;
    }
    const active = rows.filter((r) => !r.archived);
    const idx = active.indexOf(current);
    if (idx < 0) return;
    const target = active[idx + dir];
    if (!target) {
      // Special case: at the top of a named section with nothing
      // above. The cross-section branch normally handles "leave my
      // section" by routing through the row above, but when there
      // are no unsectioned rows and no other section preceding this
      // one, there's no row above to route through. Manufacture the
      // move into unsectioned so Shift+K can always escape a
      // section. (No symmetric Shift+J fix: archived is the boundary
      // below, and routing into archived via reorder would conflict
      // with `a` being the only path there.)
      if (dir < 0 && current.section !== null) {
        const slug = current.wt.slug;
        placeSlug(slug, null, "bottom").then(
          () => toast("moved to (none)", theme.info, 1200),
          (err) => reportActionError("move", err),
        );
        return;
      }
      toast(dir > 0 ? "already at bottom" : "already at top", theme.fgDim, 1500);
      return;
    }
    const slug = current.wt.slug;
    if (target.section === current.section) {
      const bucket = active
        .filter((r) => r.section === current.section)
        .map((r) => r.wt.slug);
      swapOrder(slug, target.wt.slug, current.section, bucket).catch((err) =>
        reportActionError("reorder", err),
      );
      return;
    }
    // Cross-section: place at the edge of `target.section` adjacent to
    // the source section, so the row shifts one visual position.
    const position: "top" | "bottom" = dir > 0 ? "top" : "bottom";
    placeSlug(slug, target.section, position).then(
      () => {
        const label = target.section === null ? "(none)" : target.section;
        toast(`moved to ${label}`, theme.info, 1200);
      },
      (err) => reportActionError("move", err),
    );
  }

  /**
   * `{` / `}` — shift the *section* containing the current row one
   * slot up or down in `sectionsOrder`. Unsectioned rows can't move
   * (they're pinned to the top); archived rows ignore section order.
   * Boundary cases toast explicitly so a silent no-op can't read as a
   * phantom move.
   */
  function doMoveSection(dir: -1 | 1): void {
    if (!current) return;
    if (current.archived) {
      toast("archived rows don't have a section", theme.fgDim, 1500);
      return;
    }
    if (current.section === null) {
      toast("unsectioned rows are pinned to the top", theme.fgDim, 1500);
      return;
    }
    if (filter) {
      toast("clear filter to reorder sections", theme.warn, 1500);
      return;
    }
    const name = current.section;
    appLog.event.dim(`moveSection enter name="${name}" dir=${dir}`);
    moveSection(name, dir).then(
      (moved) => {
        appLog.event.dim(`moveSection result name="${name}" dir=${dir} moved=${moved}`);
        if (!moved) {
          toast(
            dir > 0 ? "section already at bottom" : "section already at top",
            theme.fgDim,
            1500,
          );
        }
      },
      (err) => reportActionError("move section", err),
    );
  }

  function openSectionPicker(): void {
    if (!current) return;
    if (current.archived) {
      toast("archived rows don't have a section context, use `a` to restore", theme.fgDim, 2000);
      return;
    }
    if (filter) {
      toast("clear filter to set section", theme.warn, 1500);
      return;
    }
    const items = buildSectionItems(current.section);
    // Default cursor: sticky last-move-target if it's still in the
    // list (and isn't the current section), else the first item.
    // The user's most common workflow is "move several rows into the
    // same section", and forcing them to re-aim every time eats keys.
    let initial = 0;
    if (lastMoveTarget !== null && lastMoveTarget !== current.section) {
      const i = items.findIndex(
        (it) => it.kind === "section" && it.name === lastMoveTarget,
      );
      if (i >= 0) initial = i;
    }
    setModal({
      kind: "sectionPicker",
      title: `move ${current.wt.slug} to section`,
      slug: current.wt.slug,
      items,
      index: initial,
      newName: null,
    });
  }

  function commitSectionPick(item: SectionPickerItem, slug: string): void {
    if (item.kind === "none") {
      setSection(slug, null).then(
        () => toast("moved to (none)", theme.info, 1500),
        (err) => reportActionError("move", err),
      );
      setLastMoveTarget(null);
      setModal(null);
      return;
    }
    if (item.kind === "section") {
      const target = item.name;
      setSection(slug, target).then(
        () => toast(`moved to ${target}`, theme.info, 1500),
        (err) => reportActionError("move", err),
      );
      setLastMoveTarget(target);
      setModal(null);
      return;
    }
    // "+ new section" — switch to input mode. Submission lives in the
    // keyboard handler.
    setModal((m) =>
      m?.kind === "sectionPicker" ? { ...m, newName: "" } : m,
    );
  }

  /**
   * Open the rename prompt for the current row's section. No-op for
   * unsectioned and archived rows — there's no nameable section to
   * rename in those contexts.
   */
  function openSectionRename(): void {
    if (!current || current.archived) return;
    if (filter) {
      toast("clear filter to rename", theme.warn, 1500);
      return;
    }
    if (current.section === null) {
      toast("cursor is in (none), nothing to rename", theme.fgDim, 1500);
      return;
    }
    setPendingRename(current.section);
    setFooter({
      kind: "input",
      prompt: `rename "${current.section}":`,
      value: current.section,
      purpose: "rename-section",
    });
  }

  async function doRemove(slug: string): Promise<void> {
    const log = createLogger(slug);
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row) return;
    // Authoritative busy check via on-disk flock. Beats relying on the
    // cached lock query, which can still read "clean" for ~600ms after a
    // prior `d` dispatched its background destroy.
    const lock = lockStatus(slug);
    if (lock) {
      const label = lockLabel(lock);
      log.event.warn(`refused: ${label}`);
      toast(`${slug} is ${label}`, theme.warn, 2000);
      return;
    }
    if ((row.fields.dirty.data?.length ?? 0) > 0) {
      log.event.err("refused: uncommitted changes, use `wt rm <slug> --force` from shell");
      toast(`${slug} has uncommitted changes`, theme.err, 3000);
      return;
    }
    const unpushed = row.fields.sync.data?.remote?.ahead ?? 0;
    if (unpushed > 0) {
      const plural = unpushed === 1 ? "" : "s";
      log.event.err(
        `refused: ${unpushed} unpushed commit${plural}, use \`wt rm ${slug} --force\` from shell`,
      );
      toast(`${slug} has ${unpushed} unpushed commit${plural}`, theme.err, 3000);
      return;
    }
    // Tuck the row into the archived section for the duration of the
    // destroy — keeps the active list uncluttered while tail output
    // spills into the activity pane. The archive entry intentionally
    // outlives the destroy: removeWorktree leaves archive.json alone so
    // the row keeps its archived styling until it actually disappears
    // from the worktree list (driven by the lock-released → invalidate
    // worktrees trigger in useWorktreeRows). Stale entries are reaped
    // at next startup; re-creating the same slug clears the entry via
    // createWorktree.
    archive(slug);
    // Mark any in-flight action as killed in the registry first, so
    // the activity pane reads "killed" rather than the "failed" the
    // wrapper's exit code would otherwise produce. Has to happen
    // before killAllSessionsFor below — once tmux drops the session
    // out from under the wrapper there's no way for the registry to
    // distinguish "user destroyed worktree" from "wrapper crashed".
    actionRegistry.kill(slug);
    // Tear down any interactive sessions (claude, diff, shell) BEFORE
    // the worktree removal starts. Their cwds are inside the worktree;
    // letting the remove race against a live tmux child can leave it
    // writing into a half-deleted directory. killAllSessionsFor is
    // idempotent and fast (just SIGHUPs the tmux session daemons).
    // Awaited so spawnBackgroundRemove only starts once they're gone.
    try {
      await killAllSessionsFor(slug);
      void refreshTmuxSessions();
    } catch (err) {
      log.warn("kill session before remove failed", {
        err: err instanceof Error ? err.message : String(err),
      });
      // Don't block the destroy on a kill failure — worst case the
      // session is already dead, or it'll get reaped on next startup.
    }
    // Drop the shell-tail log now that the session is gone — the
    // startup reap would catch it eventually, but cleaning up at the
    // source keeps the cache dir tidy without waiting for a restart.
    removeShellLog(slug);
    spawnBackgroundRemove(slug, {
      force: false,
      destroyStage: row.fields.deploy.data ?? false,
      deleteBranch: true,
    });
    log.event.info("dispatched destroy");
    toast(`dispatched destroy of ${slug}`, theme.info);
    setTimeout(() => void invalidateWorktree(slug), 600);
  }

  async function doClean(): Promise<void> {
    const candidates = rows.filter((r) => isCleanCandidate(r));
    if (candidates.length === 0) {
      appLog.event.dim("clean: nothing to clean");
      toast("nothing to clean", theme.fgDim, 1500);
      return;
    }
    appLog.event.info(
      `clean: dispatching ${candidates.length} destroy${candidates.length === 1 ? "" : "s"}`,
    );
    // Kill every candidate's tmux sessions (every kind) before
    // dispatching any remove — same rationale as `doRemove`: don't
    // let the remove race against a live child with cwd inside the
    // worktree. Done in parallel since each kill is independent.
    // Notify the action registry first (synchronous, fast) so the
    // activity pane reads "killed" rather than the "failed" the
    // wrapper's exit code would otherwise produce.
    for (const row of candidates) actionRegistry.kill(row.wt.slug);
    await Promise.allSettled(
      candidates.map((row) => killAllSessionsFor(row.wt.slug)),
    );
    void refreshTmuxSessions();
    for (const row of candidates) {
      archive(row.wt.slug);
      removeShellLog(row.wt.slug);
      spawnBackgroundRemove(row.wt.slug, {
        force: false,
        destroyStage: row.fields.deploy.data ?? false,
        deleteBranch: true,
      });
      createLogger(row.wt.slug).event.info("dispatched destroy (clean)");
    }
    setTimeout(() => void refreshAll(), 600);
  }

  /**
   * Attach to (or create) a claude session for `slug`. `name = null`
   * is the primary; a string is one of the named sessions. Suspends
   * the renderer, hands the terminal to tmux, surfaces lifecycle
   * events to the activity pane. Toasts on spawn-fail; the rest is
   * background.
   */
  function doEnterClaudeSession(slug: string, name: string | null): void {
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row) {
      toast(`no row for ${slug}`, theme.warn, 1500);
      return;
    }
    if (row.status.kind === StatusKind.Busy) {
      toast(`${slug} is busy`, theme.warn, 2000);
      return;
    }
    const sessionLog = createLogger(slug);
    void (async () => {
      const label = name === null ? "" : ` "${name}"`;
      sessionLog.event.info(`entering claude session${label} (F12 to detach)`);
      const result = await enterClaudeSession({
        renderer,
        slug,
        cwd: row.wt.path,
        claudeName: name,
      });
      void refreshTmuxSessions();
      if (result.kind === "spawn-failed") {
        sessionLog.event.err(`claude failed to start: ${result.reason}`);
        toast(`claude failed: ${result.reason}`, theme.err, 3000);
      } else if (result.kind === "detached") {
        sessionLog.event.info(`detached from ${claudeSessionName(slug, name)}`);
      } else {
        sessionLog.event.info(`claude exited (${result.code ?? "?"})`);
        if (result.stderr) sessionLog.event.err(result.stderr);
      }
    })();
  }

  /**
   * Spawn-and-attach a brand new named claude session for `slug`.
   * `name` is presumed already validated (caller layer enforces).
   * Persists the name so the session shows up in the picker as a
   * ghost when tmux is dead but the conversation jsonl survives.
   * If `name` already exists in state, this is a resume (no
   * duplicate state mutation).
   *
   * Persist-before-spawn is intentional: a wt crash mid-spawn must
   * leave the name reachable on next start. The trade-off is a
   * spawn-failure window where we'd persist a name for a session
   * that never started; we roll that back below by only persisting
   * fresh names (not pre-existing ones) and removing on spawn-fail.
   */
  function doSpawnNamedClaudeSession(slug: string, name: string): void {
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row) {
      toast(`no row for ${slug}`, theme.warn, 1500);
      return;
    }
    if (row.status.kind === StatusKind.Busy) {
      toast(`${slug} is busy`, theme.warn, 2000);
      return;
    }
    const wasPersisted = nameInUse(slug, name);
    addClaudeName(slug, name);
    const sessionLog = createLogger(slug);
    void (async () => {
      sessionLog.event.info(`entering claude session "${name}" (F12 to detach)`);
      const result = await enterClaudeSession({
        renderer,
        slug,
        cwd: row.wt.path,
        claudeName: name,
      });
      void refreshTmuxSessions();
      if (result.kind === "spawn-failed") {
        // Roll back the optimistic add IFF we created the entry —
        // if `name` was already in the file (resume case), leave it
        // so the user can retry from the picker.
        if (!wasPersisted) removeClaudeName(slug, name);
        sessionLog.event.err(`claude failed to start: ${result.reason}`);
        toast(`claude failed: ${result.reason}`, theme.err, 3000);
      } else if (result.kind === "detached") {
        sessionLog.event.info(`detached from ${claudeSessionName(slug, name)}`);
      } else {
        sessionLog.event.info(`claude exited (${result.code ?? "?"})`);
        if (result.stderr) sessionLog.event.err(result.stderr);
      }
    })();
  }

  /**
   * Kill a claude session for `slug`. `null` = primary (jsonl is
   * preserved; next F12 attaches via --resume). String = a named
   * session; we also drop it from the persistent name list so the
   * picker stops listing it as a ghost. Idempotent.
   */
  function doKillClaudeSession(slug: string, name: string | null): void {
    // Optimistically drop the entry from `tmuxSessionsQuery` cache
    // BEFORE awaiting the kill so the picker / row badge reflect
    // immediately. Without this, a user reopening `;` in the
    // ~hundreds-of-ms window between dispatch and tmux completion
    // would still see the dying session as live and pressing Enter
    // would `tmux new-session -A` it back to life.
    optimisticRemoveClaude(slug, name);
    if (name !== null) removeClaudeName(slug, name);
    void (async () => {
      try {
        if (name === null) {
          await killSession(slug);
          appLog.event.warn(`killed primary claude session on ${slug}`);
        } else {
          await killClaudeNamedSession(slug, name);
          appLog.event.warn(`killed claude session "${name}" on ${slug}`);
        }
        void refreshTmuxSessions();
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        appLog.event.err(`kill claude session failed for ${slug}: ${msg}`);
        // Refetch to reconcile against truth — the optimistic remove
        // is wrong if the kill genuinely failed.
        void refreshTmuxSessions();
      }
    })();
  }

  /**
   * Copy `value` to the clipboard, log + toast appropriately. Used by
   * the yank chord (branch / stage / path); each item picks its own
   * label and value so the user-facing message is consistent.
   */
  function doYank(slug: string, label: string, value: string | null): void {
    const log = createLogger(slug);
    if (!value) {
      log.event.warn(`nothing to yank: ${label}`);
      toast(`no ${label} to yank`, theme.warn, 1500);
      return;
    }
    try {
      writeClipboard(value);
    } catch (err) {
      log.event.err(`pbcopy failed: ${err instanceof Error ? err.message : String(err)}`);
      log.error(err instanceof Error ? err : String(err));
      return;
    }
    log.event.info(`yanked ${label}: ${value}`);
    toast(`copied ${label}`, theme.info, 1500);
  }

  async function openReviewerPicker(slug: string): Promise<void> {
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row?.pr) {
      toast("no PR for this row", theme.warn, 2000);
      return;
    }
    if (row.pr.state !== "OPEN") {
      toast("PR is not open", theme.warn, 2000);
      return;
    }
    if (row.pr.isDraft) {
      toast("PR is a draft (mark ready first)", theme.warn, 2000);
      return;
    }
    // `fetchContributors` returns cached data without awaiting when
    // warm (background refresh when stale). Only the first-ever open
    // pays a fetch; after that the picker opens instantly even when
    // the cached list is stale. `fetchMe` is process-cached after
    // first call.
    const [contributors, me] = await Promise.all([
      fetchContributors(),
      fetchMe(),
    ]);
    const requested = new Set(row.pr.requestedReviewers);
    // Three-tier candidate list:
    //   1. PR-scoped suggestions (highest signal — file ownership +
    //      history). Often empty on small diffs.
    //   2. Already-requested logins/teams not in (1), so the picker
    //      doubles as a way to *remove* them.
    //   3. Repo-wide contributors as the fallback so the picker is
    //      never empty just because (1) was. Cached for 24h.
    const items: MultiPickerItem[] = [];
    const seen = new Set<string>();
    const skipSelf = (login: string) => me !== null && login === me;
    for (const s of row.pr.suggestedReviewers) {
      if (skipSelf(s.login)) continue;
      const already = requested.has(s.login);
      const tags: string[] = [];
      if (already) tags.push("requested");
      tags.push("suggested");
      if (s.isAuthor) tags.push("author");
      if (s.isCommenter) tags.push("commenter");
      items.push({
        key: s.login,
        label: s.login,
        hint: `(${tags.join(", ")})`,
      });
      seen.add(s.login);
    }
    for (const login of row.pr.requestedReviewers) {
      if (seen.has(login)) continue;
      if (skipSelf(login)) continue;
      items.push({ key: login, label: login, hint: "(requested)" });
      seen.add(login);
    }
    for (const c of contributors) {
      if (seen.has(c.login)) continue;
      if (skipSelf(c.login)) continue;
      items.push({
        key: c.login,
        label: c.login,
        hint: `(${c.contributions} commits)`,
      });
      seen.add(c.login);
    }
    if (items.length === 0) {
      toast("no reviewer candidates", theme.warn, 2000);
      return;
    }
    setModal({
      kind: "reviewerPicker",
      title: `edit reviewers for #${row.pr.number}`,
      items,
      index: 0,
      checked: new Set(requested),
      original: new Set(requested),
      slug,
      prNumber: row.pr.number,
    });
  }

  async function submitReviewerPicker(): Promise<void> {
    if (modal?.kind !== "reviewerPicker") return;
    const { slug, prNumber, checked, original } = modal;
    const log = createLogger(slug);
    const branch = rows.find((r) => r.wt.slug === slug)?.wt.branch;
    setModal(null);
    if (!branch) {
      // Slug disappeared between picker open and submit (race against
      // a destroy). The mutation would still succeed at the gh layer,
      // but the optimistic patch has nothing to target — bail rather
      // than silently dropping the cache update.
      log.event.warn(`slug ${slug} no longer present; aborting reviewer edit`);
      toast("worktree gone, edit aborted", theme.warn, 2500);
      return;
    }
    const add: string[] = [];
    const remove: string[] = [];
    for (const k of checked) if (!original.has(k)) add.push(k);
    for (const k of original) if (!checked.has(k)) remove.push(k);
    if (add.length === 0 && remove.length === 0) {
      toast("no changes", theme.fgDim, 1500);
      return;
    }
    try {
      await mutate<GithubData>({
        filter: { queryKey: ["github"] },
        patch: (data) =>
          patchPullRequest(data, branch, (pr) => ({
            ...pr,
            requestedReviewers: [...checked],
            reviewRequests: pr.reviewRequests + add.length - remove.length,
          })),
        run: async () => {
          const result = await editReviewers(prNumber, { add, remove });
          if (!result.ok) throw new Error(result.error);
        },
      });
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      log.event.err(`edit reviewers failed for #${prNumber}: ${msg}`);
      toast(`edit reviewers failed: ${msg}`, theme.err, 4000);
      return;
    }
    const parts: string[] = [];
    if (add.length > 0) parts.push(`+${add.join(", ")}`);
    if (remove.length > 0) parts.push(`-${remove.join(", ")}`);
    log.event.ok(`edited reviewers for #${prNumber}: ${parts.join("; ")}`);
    const summary = [
      add.length > 0 ? `added ${add.length}` : null,
      remove.length > 0 ? `removed ${remove.length}` : null,
    ]
      .filter(Boolean)
      .join(", ");
    toast(summary, theme.ok, 2500);
  }

  async function doMarkReady(slug: string): Promise<void> {
    const log = createLogger(slug);
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row?.pr) {
      toast("no PR for this row", theme.warn, 2000);
      return;
    }
    const prNumber = row.pr.number;
    const branch = row.wt.branch;
    try {
      await mutate<GithubData>({
        filter: { queryKey: ["github"] },
        patch: (data) =>
          patchPullRequest(data, branch, (pr) => ({ ...pr, isDraft: false })),
        run: async () => {
          const result = await markPullRequestReady(prNumber);
          if (!result.ok) throw new Error(result.error);
        },
      });
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      log.event.err(`mark ready failed for #${prNumber}: ${msg}`);
      toast(`mark ready failed: ${msg}`, theme.err, 4000);
      return;
    }
    log.event.ok(`marked #${prNumber} ready for review`);
    toast(`marked #${prNumber} ready`, theme.ok, 2500);
  }

  /**
   * Arm Graphite's "merge when ready" via `gt submit -m`. No optimistic
   * patch — we don't currently know which `mergeabilityStatus` value
   * Graphite returns for an armed PR, so there's nothing to flip in the
   * cache. The settling invalidate of the graphite query picks up the
   * change on the next round-trip.
   *
   * One-way arm: there's no documented disarm path through `gt` or
   * Graphite's API. The toast directs the user to the web UI for that.
   */
  async function doMergeWhenReady(slug: string): Promise<void> {
    const log = createLogger(slug);
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row?.pr) {
      toast("no PR for this row", theme.warn, 2000);
      return;
    }
    const prNumber = row.pr.number;
    const wtPath = row.wt.path;
    log.event.dim(`arming merge-when-ready for #${prNumber}...`);
    const result = await armMergeWhenReady(wtPath);
    if (!result.ok) {
      log.event.err(`merge-when-ready failed for #${prNumber}: ${result.error}`);
      toast(`merge-when-ready failed: ${result.error}`, theme.err, 4000);
      return;
    }
    log.event.ok(`armed merge-when-ready for #${prNumber}`);
    toast(
      `armed #${prNumber} · disarm at app.graphite.com`,
      theme.ok,
      4000,
    );
    void refreshGraphite();
    void refreshGithub();
  }

  function buildActionPickerItems(slug: string): PickerItem[] {
    const row = rows.find((r) => r.wt.slug === slug);
    const rowState = { pr: row?.pr };
    return [
      ...config.actions.map((def) => ({
        kind: "action" as const,
        def,
        availability: evaluateActionRequirements(def.requires, rowState),
      })),
      { kind: "custom" as const },
    ];
  }

  /**
   * Returns true if the item is launchable. For unavailable actions
   * toasts the reason so the user understands the no-op without
   * having to scan the dim subtitle in the picker. Used at both the
   * Enter and quick-pick-digit handlers so an unavailable action
   * can't slip into the edit modal.
   */
  function canPickAction(item: PickerItem): boolean {
    if (item.kind === "custom") return true;
    if (item.availability.ok) return true;
    toast(`${item.def.name}: ${item.availability.reason}`, theme.warn, 2500);
    return false;
  }

  function openActionPicker(slug: string): void {
    setModal({
      kind: "actionPicker",
      state: { mode: "list", slug, index: 0 },
    });
  }

  function launchAction(
    slug: string,
    def: ActionDef | null,
    extras: string,
  ): void {
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row) {
      toast("worktree gone", theme.warn, 1500);
      return;
    }
    if (!def && !extras.trim()) {
      toast("prompt is empty", theme.warn, 1500);
      return;
    }
    // Refuse if the worktree is mid-destroy / mid-init — claude would
    // race the cleanup and leave the tree in a confusing state. Mirrors
    // the `doRemove` / `doNew` busy refusal pattern.
    const lock = lockStatus(slug);
    if (lock) {
      toast(`${slug} is ${lockLabel(lock)}`, theme.warn, 2000);
      return;
    }
    if (row.status.kind === StatusKind.Busy) {
      toast(`${slug} is busy`, theme.warn, 2000);
      return;
    }
    if (def) {
      const avail = evaluateActionRequirements(def.requires, { pr: row.pr });
      if (!avail.ok) {
        toast(`${def.name}: ${avail.reason}`, theme.warn, 2500);
        return;
      }
    }
    const vars = buildActionVars(row);
    const result = def
      ? actionRegistry.start(def, slug, row.wt.path, extras, vars)
      : actionRegistry.startCustom(slug, row.wt.path, extras, vars);
    if (!result.ok) {
      toast(`action: ${result.reason}`, theme.err, 3000);
      return;
    }
    // Clear this worktree's focus so the auto-rules surface the
    // just-launched action. Per-slug pin survives if set — the user
    // explicitly pinned something else for this slug; respect it.
    const bucket = slugFocus[slug] ?? EMPTY_FOCUS;
    if (!bucket.pinned) setFocus(slug, { focused: null });
    toast(`launched ${result.run.actionName}`, theme.info, 2000);
  }

  async function doNew(raw: string, defaultBase?: string): Promise<void> {
    const parsed = parseNewInput(raw, defaultBase);
    if ("error" in parsed) {
      newLog.event.err(parsed.error);
      return;
    }
    newLog.event.info(`resolving ${parsed.input}`);
    if (parsed.anyAuthor) newLog.event.info("searching all authors (--any)");
    if (parsed.base) newLog.event.info(`base: ${parsed.base}`);
    let branch: string;
    try {
      branch = await parseInput(parsed.input, {
        anyAuthor: parsed.anyAuthor,
        promptForChoice: (id, branches) =>
          new Promise<string | null>((resolve) => {
            setModal({
              kind: "branchPicker",
              title: `multiple branches for ${id}`,
              items: branches,
              index: 0,
              resolve,
            });
          }),
      });
    } catch (err) {
      newLog.event.err(err instanceof Error ? err.message : String(err));
      newLog.error(err instanceof Error ? err : String(err));
      return;
    }
    newLog.event.info(`branch = ${branch}`);
    const result = await createWorktree(branch, {
      onPhase: (p) => newLog.event.info(`phase: ${p}`),
      onLog: (line) => newLog.event.dim(line),
      runInstall: true,
      base: parsed.base,
    });
    if (!result.ok) {
      newLog.event.err(result.reason);
      return;
    }
    newLog.event.ok(`ready at ${result.path}`);
    void refreshAll();
  }

  useKeyboard((k) => {
    // Help overlay swallows input while open.
    if (modal?.kind === "help") {
      if (
        k.name === "escape" ||
        k.sequence === "?" ||
        k.name === "q" ||
        (k.ctrl && k.name === "c")
      ) {
        setModal(null);
      }
      return;
    }

    // Reviewer multi-picker. Space toggles the cursor item, enter
    // submits the checked set, esc cancels.
    if (modal?.kind === "reviewerPicker") {
      const rp = modal;
      if (k.name === "j" || k.name === "down") {
        setModal({
          ...rp,
          index: Math.min(rp.index + 1, rp.items.length - 1),
        });
        return;
      }
      if (k.name === "k" || k.name === "up") {
        setModal({ ...rp, index: Math.max(rp.index - 1, 0) });
        return;
      }
      if (k.name === "space" || k.sequence === " ") {
        const item = rp.items[rp.index];
        if (item) {
          const next = new Set(rp.checked);
          if (next.has(item.key)) next.delete(item.key);
          else next.add(item.key);
          setModal({ ...rp, checked: next });
        }
        return;
      }
      if (k.name === "return") {
        void submitReviewerPicker();
        return;
      }
      if (
        k.name === "escape" ||
        k.sequence === "v" ||
        k.sequence === "q" ||
        (k.ctrl && k.name === "c")
      ) {
        setModal(null);
      }
      return;
    }

    // Section picker (`l`). Two modes: list mode for picking an
    // existing section / "(none)" / "+ new section", and input mode
    // for typing the new section name when the user picks the create
    // entry. `newName === null` means list mode.
    if (modal?.kind === "sectionPicker") {
      const sp = modal;
      if (sp.newName !== null) {
        if (k.name === "escape") {
          setModal({ ...sp, newName: null });
          return;
        }
        if (k.ctrl && k.name === "c") {
          setModal(null);
          return;
        }
        if (k.name === "return") {
          const name = sp.newName.trim();
          if (!name) {
            setModal({ ...sp, newName: null });
            return;
          }
          const slug = sp.slug;
          setSection(slug, name).then(
            () => toast(`moved to ${name}`, theme.info, 1500),
            (err) => reportActionError("move", err),
          );
          setLastMoveTarget(name);
          setModal(null);
          return;
        }
        if (k.name === "backspace") {
          // Backspace on empty input pops back to list mode — matches
          // the filter / new-worktree input convention.
          if (sp.newName.length === 0) {
            setModal({ ...sp, newName: null });
            return;
          }
          setModal({ ...sp, newName: sp.newName.slice(0, -1) });
          return;
        }
        const text = printableText(k.sequence);
        if (text) setModal({ ...sp, newName: sp.newName + text });
        return;
      }
      if (k.name === "j" || k.name === "down") {
        setModal({
          ...sp,
          index: Math.min(sp.index + 1, sp.items.length - 1),
        });
        return;
      }
      if (k.name === "k" || k.name === "up") {
        setModal({ ...sp, index: Math.max(sp.index - 1, 0) });
        return;
      }
      // `l` inside the picker is the chord trigger for "+ new section"
      // (so `l l` from normal mode lands you in the create-name
      // input). Plain `l`, no modifiers — Shift+L doesn't apply here
      // since rename is only a normal-mode action.
      if (isPlainLetter(k, "l")) {
        const createIdx = sp.items.findIndex((it) => it.kind === "create");
        if (createIdx >= 0) {
          const item = sp.items[createIdx]!;
          commitSectionPick(item, sp.slug);
        }
        return;
      }
      // Quick-pick digits 1..9 jump straight to that item by display
      // position. Mirrors the digit prefix the modal renders. Ignored
      // when the position is out of range (so a stray "9" in a list
      // of three doesn't fire something unintended).
      if (k.sequence && /^[1-9]$/.test(k.sequence)) {
        const i = parseInt(k.sequence, 10) - 1;
        const item = sp.items[i];
        if (item) commitSectionPick(item, sp.slug);
        return;
      }
      if (k.name === "return") {
        const item = sp.items[sp.index];
        if (item) commitSectionPick(item, sp.slug);
        return;
      }
      if (
        k.name === "escape" ||
        k.sequence === "q" ||
        (k.ctrl && k.name === "c")
      ) {
        setModal(null);
      }
      return;
    }

    // Action picker — list of pre-built actions plus a trailing
    // "Custom prompt..." entry. Two screens: list mode (j/k + return),
    // then edit mode where the user types extras / a freeform prompt.
    if (modal?.kind === "actionPicker") {
      const ap = modal.state;
      if (ap.mode === "list") {
        // Recompute items here rather than reading from state — they
        // depend on live row state via `requires`, and a stale snapshot
        // would block actions whose preconditions just flipped (or
        // unblock ones that just stopped applying).
        const items = buildActionPickerItems(ap.slug);
        if (k.name === "j" || k.name === "down") {
          setModal({
            kind: "actionPicker",
            state: { ...ap, index: Math.min(ap.index + 1, items.length - 1) },
          });
          return;
        }
        if (k.name === "k" || k.name === "up") {
          setModal({
            kind: "actionPicker",
            state: { ...ap, index: Math.max(ap.index - 1, 0) },
          });
          return;
        }
        // `!` chord — jumps straight to the custom-prompt entry. Mirrors
        // the section picker's `l → "+ new section"` chord, so `! !`
        // from normal mode lands directly in freeform-edit mode.
        if (k.sequence === "!") {
          setModal({
            kind: "actionPicker",
            state: { mode: "edit", slug: ap.slug, def: null, extras: "" },
          });
          return;
        }
        // Quick-pick digits 1..9 jump straight to that *action* item
        // (the custom entry is reachable via `!`, not a digit). Out-of-
        // range digits are ignored so a stray "9" in a list of three
        // doesn't fire something unintended.
        if (k.sequence && /^[1-9]$/.test(k.sequence)) {
          const i = parseInt(k.sequence, 10) - 1;
          const item = items[i];
          if (item && item.kind === "action") {
            if (!canPickAction(item)) return;
            if (item.def.kind === "shell") {
              setModal(null);
              launchAction(ap.slug, item.def, "");
            } else {
              setModal({
                kind: "actionPicker",
                state: { mode: "edit", slug: ap.slug, def: item.def, extras: "" },
              });
            }
          }
          return;
        }
        if (k.name === "return") {
          const item = items[ap.index];
          if (!item) return;
          if (!canPickAction(item)) return;
          if (item.kind === "action" && item.def.kind === "shell") {
            setModal(null);
            launchAction(ap.slug, item.def, "");
            return;
          }
          // Custom (no def) or claude action → into the edit modal.
          const def = item.kind === "action" ? item.def : null;
          setModal({
            kind: "actionPicker",
            state: {
              mode: "edit",
              slug: ap.slug,
              def: def && def.kind === "claude" ? def : null,
              extras: "",
            },
          });
          return;
        }
        if (
          k.name === "escape" ||
          k.sequence === "q" ||
          (k.ctrl && k.name === "c")
        ) {
          setModal(null);
        }
        return;
      }
      // mode === "edit"
      if (k.ctrl && k.name === "c") {
        setModal(null);
        return;
      }
      if (k.name === "escape") {
        // Pre-built path: pop back to the list at the same item the user
        // selected. Custom path: there's no informative list state to
        // restore (custom is the only entry there), so esc cancels out.
        const def = ap.def;
        if (def) {
          // Worktree may have been destroyed while the edit modal was
          // open. Bail to a clean exit rather than dropping the user
          // into a phantom-slug list whose entries all read "no PR".
          if (!rows.find((r) => r.wt.slug === ap.slug)) {
            setModal(null);
            toast("worktree gone", theme.warn, 2000);
            return;
          }
          const items = buildActionPickerItems(ap.slug);
          const idx = items.findIndex(
            (it) => it.kind === "action" && it.def.id === def.id,
          );
          setModal({
            kind: "actionPicker",
            state: { mode: "list", slug: ap.slug, index: Math.max(0, idx) },
          });
        } else {
          setModal(null);
        }
        return;
      }
      if (k.name === "return") {
        const { slug, def, extras } = ap;
        setModal(null);
        launchAction(slug, def, extras);
        return;
      }
      if (k.name === "backspace") {
        if (ap.extras.length === 0) return;
        setModal({
          kind: "actionPicker",
          state: { ...ap, extras: ap.extras.slice(0, -1) },
        });
        return;
      }
      const text = printableMultiline(k.sequence);
      if (text) {
        setModal({
          kind: "actionPicker",
          state: { ...ap, extras: ap.extras + text },
        });
      }
      return;
    }

    // Outputs picker — vim-buffer-style list of this worktree's
    // outputs (events + this slug's actions + this slug's claude
    // session). j/k or arrows move AND commit focus immediately
    // (live preview: ";jj;" lands on the third entry without a
    // return press); 1-9 quick-pick by index; Enter just closes (the
    // focus already matches); `'` toggles pin on the selected entry;
    // esc/q cancels (without revert — live commit semantics). `idx`
    // is clamped against the live `visibleOutputs` length on every
    // keypress because the underlying list can shrink while the
    // picker is open (FIFO eviction, session ending). Pin is cleared
    // on movement so the live preview actually displays — same
    // reason `[`/`]` clear pin.
    if (modal?.kind === "outputsPicker") {
      const idx =
        visibleOutputs.length === 0
          ? 0
          : Math.min(Math.max(0, modal.index), visibleOutputs.length - 1);
      const moveTo = (next: number): void => {
        setModal({ kind: "outputsPicker", index: next });
        const patch = previewFocusPatch(visibleOutputs[next]?.id ?? null);
        if (patch) setFocus(currentSlug ?? null, patch);
      };
      if (k.name === "j" || k.name === "down") {
        moveTo(Math.min(idx + 1, visibleOutputs.length - 1));
        return;
      }
      if (k.name === "k" || k.name === "up") {
        moveTo(Math.max(0, idx - 1));
        return;
      }
      if (k.sequence && /^[1-9]$/.test(k.sequence)) {
        const i = parseInt(k.sequence, 10) - 1;
        // Same pin-clearing as j/k movement — quick-pick is a
        // navigation override, so any prior pin shouldn't outvote
        // the explicit choice.
        const patch = previewFocusPatch(visibleOutputs[i]?.id ?? null);
        if (patch) {
          setFocus(currentSlug ?? null, patch);
          setModal(null);
        }
        return;
      }
      if (k.sequence === "'") {
        const patch = togglePinPatch(
          visibleOutputs[idx]?.id ?? null,
          focusBucket.pinned,
        );
        if (patch) {
          setFocus(currentSlug ?? null, patch);
          setModal(null);
        }
        return;
      }
      if (k.name === "return") {
        const target = visibleOutputs[idx];
        if (target) setFocus(currentSlug ?? null, { focused: target.id });
        setModal(null);
        return;
      }
      if (
        k.name === "escape" ||
        k.sequence === "q" ||
        k.sequence === ":" ||
        (k.ctrl && k.name === "c")
      ) {
        setModal(null);
      }
      return;
    }

    // Claude sessions picker — list view. Items are derived live on
    // each keypress from the tmux session list + persisted name list,
    // so a kill or spawn elsewhere reflects immediately when the user
    // navigates. Layout: primary first, then live named, then ghost
    // named (live in tmux = false), then "+ new".
    //
    // Live-preview semantics borrowed from the outputs picker: j/k
    // moves AND points the bottom pane at the highlighted session's
    // log so the user can scan sessions without committing. Ghost /
    // "+ new" rows leave focus alone (no live output to preview).
    // `'` pins the highlighted session's log; Enter / 1-9 attach to
    // its tmux session.
    if (modal?.kind === "claudeSessionsPicker") {
      const slug = modal.slug;
      const entries = pickerEntries;
      // Read pin/focus from `modal.slug`'s bucket, NOT
      // `currentSlug`'s. While `;` opens with the two equal, a row
      // churn under the open modal could decouple them — and then
      // the toggle would read bucket A and write to bucket B.
      const modalBucket = slugFocus[slug] ?? EMPTY_FOCUS;
      // Total interactive rows = entries + the trailing "+ new" row,
      // which is appended by the picker UI itself. The handler treats
      // index `entries.length` as that "+ new" affordance.
      const totalRows = entries.length + 1;
      const idx = Math.min(Math.max(0, modal.index), totalRows - 1);
      const previewIdFor = (i: number): string | null => {
        const target = entries[i];
        return target && target.isLive
          ? sessionOutputId(slug, "claude", target.name)
          : null;
      };
      const moveTo = (next: number): void => {
        setModal({ ...modal, index: next });
        const patch = previewFocusPatch(previewIdFor(next));
        if (patch) setFocus(slug, patch);
      };
      const select = (i: number): void => {
        if (i < 0 || i >= totalRows) return;
        if (i === entries.length) {
          setModal({
            kind: "claudeSessionsNew",
            slug,
            input: "",
            error: null,
          });
          return;
        }
        setModal(null);
        doEnterClaudeSession(slug, entries[i]!.name);
      };
      if (k.name === "j" || k.name === "down") {
        moveTo(Math.min(idx + 1, totalRows - 1));
        return;
      }
      if (k.name === "k" || k.name === "up") {
        moveTo(Math.max(0, idx - 1));
        return;
      }
      if (k.sequence && /^[1-9]$/.test(k.sequence)) {
        select(parseInt(k.sequence, 10) - 1);
        return;
      }
      if (k.sequence === "x") {
        const target = entries[idx];
        if (target) {
          if (target.isLive) {
            doKillClaudeSession(slug, target.name);
          } else if (target.name !== null) {
            // Ghost: jsonl is gone or session was already killed —
            // just drop from state so the picker stops listing it.
            removeClaudeName(slug, target.name);
            appLog.event.info(`forgot ghost session "${target.name}" on ${slug}`);
          }
          setModal(null);
        }
        return;
      }
      if (k.sequence === "'") {
        const patch = togglePinPatch(previewIdFor(idx), modalBucket.pinned);
        if (patch) {
          setFocus(slug, patch);
          setModal(null);
        }
        return;
      }
      if (k.name === "return") {
        select(idx);
        return;
      }
      if (
        k.name === "escape" ||
        k.sequence === "q" ||
        k.sequence === ";" ||
        (k.ctrl && k.name === "c")
      ) {
        setModal(null);
      }
      return;
    }

    // Claude sessions picker — new-name input phase. Accepts the
    // same chars `validateSessionName` accepts plus backspace; Enter
    // commits, esc returns to the list. Empty input on Enter spawns
    // with the auto-numbered name (next free integer ≥ 2).
    if (modal?.kind === "claudeSessionsNew") {
      if (k.name === "escape" || (k.ctrl && k.name === "c")) {
        setModal({ kind: "claudeSessionsPicker", slug: modal.slug, index: 0 });
        return;
      }
      if (k.name === "return") {
        const trimmed = modal.input.trim();
        const name = trimmed === "" ? nextAutoName(modal.slug) : trimmed;
        const err = validateSessionName(name);
        if (err) {
          setModal({ ...modal, error: err });
          return;
        }
        setModal(null);
        doSpawnNamedClaudeSession(modal.slug, name);
        return;
      }
      if (k.name === "backspace") {
        setModal({ ...modal, input: modal.input.slice(0, -1), error: null });
        return;
      }
      if (k.sequence && /^[a-zA-Z0-9_-]$/.test(k.sequence)) {
        setModal({
          ...modal,
          input: modal.input + k.sequence,
          error: null,
        });
        return;
      }
      return;
    }

    // Kill-confirm for an in-flight `!` action on the selected
    // worktree. Mirrors the y/N pattern used by clean-confirm; also
    // accepts `!` (the opening key) per the modal toggle convention.
    if (modal?.kind === "killActionConfirm") {
      if (k.name === "y" || k.name === "return") {
        const { slug, actionName } = modal;
        setModal(null);
        const killed = actionRegistry.kill(slug);
        if (killed) {
          appLog.event.warn(`killed action "${actionName}" on ${slug}`);
        }
        return;
      }
      if (
        k.name === "n" ||
        k.name === "escape" ||
        k.sequence === "!" ||
        k.sequence === "q" ||
        (k.ctrl && k.name === "c")
      ) {
        setModal(null);
      }
      return;
    }

    // Kill-confirm for an interactive tmux session on the selected
    // worktree. Mirrors `killActionConfirm`. For claude, the
    // conversation jsonl is preserved — next F12 attaches via --resume
    // to the same UUID. For diff, the next F11 opens fresh state.
    if (modal?.kind === "killSessionConfirm") {
      if (k.name === "y" || k.name === "return") {
        const { slug, sessionKind } = modal;
        setModal(null);
        const kill = sessionKind === "diff" ? killDiffSession : killShellSession;
        void kill(slug)
          .then(() => {
            appLog.event.warn(`killed ${sessionKind} session on ${slug}`);
            void refreshTmuxSessions();
          })
          .catch((err) => {
            const msg = err instanceof Error ? err.message : String(err);
            appLog.event.err(
              `kill ${sessionKind} session failed for ${slug}: ${msg}`,
            );
          });
        return;
      }
      if (
        k.name === "n" ||
        k.name === "escape" ||
        k.sequence === "q" ||
        (k.ctrl && k.name === "c")
      ) {
        setModal(null);
      }
      return;
    }

    // Branch-picker modal (for --any multi-match). Swallows input
    // until the user picks or cancels, resolving the promise that
    // `parseInput` is awaiting inside `doNew`.
    if (modal?.kind === "branchPicker") {
      const bp = modal;
      if (k.name === "j" || k.name === "down") {
        setModal({
          ...bp,
          index: Math.min(bp.index + 1, bp.items.length - 1),
        });
        return;
      }
      if (k.name === "k" || k.name === "up") {
        setModal({ ...bp, index: Math.max(bp.index - 1, 0) });
        return;
      }
      if (k.name === "return") {
        const chosen = bp.items[bp.index]!;
        bp.resolve(chosen);
        setModal(null);
        return;
      }
      if (
        k.name === "escape" ||
        k.sequence === "q" ||
        (k.ctrl && k.name === "c")
      ) {
        bp.resolve(null);
        setModal(null);
      }
      return;
    }

    // Yank chord: `y` opened the menu; the next key picks what to copy.
    // `y` again, esc, or ctrl+c cancels. Unmapped keys are ignored
    // rather than re-entering normal mode, so a stray keystroke can't
    // accidentally trigger a destructive action.
    if (modal?.kind === "yank") {
      if (
        k.name === "escape" ||
        k.sequence === "y" ||
        k.sequence === "q" ||
        (k.ctrl && k.name === "c")
      ) {
        setModal(null);
        return;
      }
      if (current) {
        const item = yankItemsFor(current).find((it) => it.key === k.sequence);
        if (item) {
          setModal(null);
          doYank(current.wt.slug, item.label, item.value);
        }
      }
      return;
    }

    // Clean-confirm modal swallows input while open.
    if (modal?.kind === "cleanConfirm") {
      if (k.name === "y" || k.name === "return") {
        setModal(null);
        void doClean();
        return;
      }
      if (
        k.name === "n" ||
        k.name === "escape" ||
        k.sequence === "q" ||
        (k.ctrl && k.name === "c")
      ) {
        setModal(null);
      }
      return;
    }

    // Filter mode: typing live-narrows the list.
    if (footer.kind === "filter") {
      if (k.name === "escape" || (k.ctrl && k.name === "c")) {
        setFilter("");
        setFooter({ kind: "legend" });
        setSel(null);
        return;
      }
      if (k.name === "return") {
        setFooter({ kind: "legend" });
        return;
      }
      if (k.name === "backspace") {
        // Backspace on an empty filter exits filter mode, matching the
        // "one more delete cancels" convention.
        if (footer.value.length === 0) {
          setFilter("");
          setFooter({ kind: "legend" });
          setSel(null);
          return;
        }
        const next = footer.value.slice(0, -1);
        setFooter({ kind: "filter", value: next });
        setFilter(next);
        setSel(null);
        return;
      }
      const text = printableText(k.sequence);
      if (text) {
        const next = footer.value + text;
        setFooter({ kind: "filter", value: next });
        setFilter(next);
        setSel(null);
      }
      return;
    }

    // Input mode: typing into the new-worktree prompt or the
    // rename-section prompt — `purpose` discriminates which.
    if (footer.kind === "input") {
      if (k.name === "escape" || (k.ctrl && k.name === "c")) {
        setFooter({ kind: "legend" });
        setPendingRename(null);
        return;
      }
      if (k.name === "return") {
        const raw = footer.value.trim();
        const base = footer.base;
        const purpose = footer.purpose;
        setFooter({ kind: "legend" });
        if (purpose === "rename-section") {
          const oldName = pendingRename;
          setPendingRename(null);
          if (!oldName || !raw || raw === oldName) return;
          renameSection(oldName, raw).catch((err) => {
            const msg = err instanceof Error ? err.message : String(err);
            appLog.event.err(`rename failed: ${msg}`);
            toast(`rename failed: ${msg}`, theme.err, 3000);
          });
          // Update sticky last-move-target so a stale name doesn't
          // dangle as the picker default.
          setLastMoveTarget((prev) => (prev === oldName ? raw : prev));
          toast(`renamed "${oldName}" to "${raw}"`, theme.info, 1800);
          return;
        }
        if (raw) void doNew(raw, base);
        return;
      }
      if (k.name === "backspace") {
        // Backspace on empty input exits, matching the filter convention.
        if (footer.value.length === 0) {
          setFooter({ kind: "legend" });
          return;
        }
        setFooter({ ...footer, value: footer.value.slice(0, -1) });
        return;
      }
      // `k.sequence` is the literal bytes the terminal delivered — a
      // single key for typing, or a paste blob. Filter to printable
      // ASCII so control chars in the middle of a paste don't corrupt.
      const text = printableText(k.sequence);
      if (text) setFooter({ ...footer, value: footer.value + text });
      return;
    }

    // Inline confirm.
    if (footer.kind === "confirm") {
      if (k.name === "y" || k.name === "return") {
        const pending = footer.pendingKey;
        setFooter({ kind: "legend" });
        if (pending === "d" && current) {
          void doRemove(current.wt.slug);
        } else if (pending === "m+" && current) {
          void doMergeWhenReady(current.wt.slug);
        } else if (pending === "e" && current) {
          void doMarkReady(current.wt.slug);
        } else if (pending === "R") {
          appLog.event.warn("cleared all cached data; refetching from scratch");
          void clearAll();
        }
        return;
      }
      if (k.name === "n" || k.name === "escape" || (k.ctrl && k.name === "c")) {
        setFooter({ kind: "legend" });
        return;
      }
      return;
    }

    // Normal mode.
    if (k.name === "escape" && filter) {
      setFilter("");
      setSel(null);
      return;
    }
    // Escape clears this worktree's explicit focus / pin so the
    // bottom pane returns to follow-row auto-rules. Filter-clear
    // above takes precedence; both can apply but they're disjoint
    // states (filter edits set the filter; output focus is per-slug
    // bucket), so the order is fine.
    if (
      k.name === "escape" &&
      (focusBucket.focused || focusBucket.pinned)
    ) {
      setFocus(currentSlug ?? null, { focused: null, pinned: null });
      return;
    }
    // `:` opens the Outputs picker — vim-`:ls` flavor for the bottom
    // pane. Cursor lands on the displayed output within this
    // worktree's filtered list. Outputs is the rarer chord; sessions
    // wins `;`.
    if (k.sequence === ":") {
      const idx = Math.max(
        0,
        indexOfOutput(visibleOutputs, displayedOutput.id),
      );
      setModal({ kind: "outputsPicker", index: idx });
      return;
    }
    // `;` opens the claude sessions picker for the current row.
    // Lists live primary + named sessions plus a "+ new" affordance
    // so the user can spawn additional named sessions in one place.
    // Refuses on rows with no current row (no slug to scope to).
    //
    // Initial cursor: if the bottom pane is currently displaying a
    // claude session for this slug, land on its row so the picker
    // mirrors what the user is already looking at. Otherwise default
    // to primary (entries[0]). Mirrors the outputs picker's "open
    // on the displayed item" pattern.
    if (k.sequence === ";") {
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      if (current.status.kind === StatusKind.Busy) {
        toast(`${current.wt.slug} is busy`, theme.warn, 2000);
        return;
      }
      const slug = current.wt.slug;
      const entries = buildClaudeSessionEntries(
        slug,
        claudeSessionsBySlug.get(slug) ?? [],
      );
      let initialIdx = 0;
      if (
        displayedOutput.kind === "session" &&
        displayedOutput.sessionKind === "claude" &&
        displayedOutput.slug === slug
      ) {
        const matchIdx = entries.findIndex(
          (e) => e.name === displayedOutput.sessionName,
        );
        if (matchIdx >= 0) initialIdx = matchIdx;
      }
      setModal({ kind: "claudeSessionsPicker", slug, index: initialIdx });
      return;
    }
    // `[` / `]` — cycle prev/next through THIS worktree's visible
    // outputs. Wraps at both ends. Clears any pin so the cycle
    // actually moves the displayed output; otherwise pin > focus
    // and the keypress would be a silent no-op (same trap `~`
    // explicitly handles).
    if (k.sequence === "[" || k.sequence === "]") {
      if (visibleOutputs.length === 0) return;
      const cur = Math.max(
        0,
        indexOfOutput(visibleOutputs, displayedOutput.id),
      );
      const step = k.sequence === "]" ? 1 : -1;
      const next =
        (cur + step + visibleOutputs.length) % visibleOutputs.length;
      const target = visibleOutputs[next];
      if (target) {
        setFocus(currentSlug ?? null, {
          focused: target.id,
          pinned: null,
        });
      }
      return;
    }
    // `"` jumps to events for this worktree's bucket — the global
    // pane when you want to step out of whatever per-row context the
    // auto-rules surfaced. Clears pin so the jump actually takes
    // effect; otherwise pin > focus and the keypress would be a
    // silent no-op.
    if (k.sequence === '"') {
      setFocus(currentSlug ?? null, {
        focused: eventsOutputId(),
        pinned: null,
      });
      return;
    }
    // `'` toggles pin on the current displayed output for THIS
    // worktree. Pin overrides auto-rules and other focus changes for
    // this slug; press again to unpin and resume auto.
    //
    // Edge case: if the bucket's pinned id no longer resolves to a
    // visible output (action FIFO'd, session ended) we're in a
    // "dangling pin" window before the GC sweep clears it. Treat
    // dangling as "currently unpinned" so the user's first `'`
    // reliably ends in unpinned, instead of pinning the
    // auto-resolved fallback (typically events).
    if (k.sequence === "'") {
      const id = displayedOutput.id;
      const pinned = focusBucket.pinned;
      const pinResolves =
        pinned !== null && visibleOutputs.some((o) => o.id === pinned);
      const togglingOff = pinResolves && pinned === id;
      setFocus(currentSlug ?? null, {
        focused: id,
        pinned: togglingOff ? null : id,
      });
      return;
    }
    // Unified Shift+J/K — moves the current row one position in
    // display order. Within a section that's a swap; across the
    // section boundary it's an adjacent-edge placement (top of next,
    // bottom of prev), so chord-holding J walks the row through the
    // whole list including unsectioned, never crossing into archived.
    if (isShiftedLetter(k, "j")) {
      doShiftMove(1);
      return;
    }
    if (isShiftedLetter(k, "k")) {
      doShiftMove(-1);
      return;
    }
    // Shift+L renames the current row's section.
    if (isShiftedLetter(k, "l")) {
      openSectionRename();
      return;
    }
    // `{` / `}` — shift the current row's section up/down in the
    // section list. Sibling rows move with it; member ordering within
    // the section is preserved.
    if (k.sequence === "{") {
      doMoveSection(-1);
      return;
    }
    if (k.sequence === "}") {
      doMoveSection(1);
      return;
    }
    if (k.name === "j" || k.name === "down") {
      if (filteredRows.length === 0) return;
      const nextIdx = Math.min(cursorIndex + 1, filteredRows.length - 1);
      setSel(filteredRows[nextIdx]?.wt.slug ?? null);
      return;
    }
    if (k.name === "k" || k.name === "up") {
      if (filteredRows.length === 0) return;
      const nextIdx = Math.max(0, cursorIndex - 1);
      setSel(filteredRows[nextIdx]?.wt.slug ?? null);
      return;
    }
    // The raw-stdin keypress parser lowercases `name` for A–Z and sets
    // `shift: true`, so case-sensitive bindings (`g`/`G`, `r`/`R`) have
    // to disambiguate on `sequence` rather than `name`.
    if (k.sequence === "g") {
      setSel(filteredRows[0]?.wt.slug ?? null);
      return;
    }
    if (k.sequence === "G") {
      setSel(filteredRows[filteredRows.length - 1]?.wt.slug ?? null);
      return;
    }
    if (isPlainLetter(k, "q") || (k.ctrl && k.name === "c")) {
      quit();
      return;
    }
    if (k.sequence === "?") {
      setModal({ kind: "help" });
      return;
    }
    if (k.sequence === "/") {
      setFooter({ kind: "filter", value: filter });
      return;
    }
    if (k.sequence === "r") {
      appLog.event.dim("refresh");
      void refreshAll();
      return;
    }
    if (k.sequence === "R") {
      setFooter({
        kind: "confirm",
        message: "clear all cached data? [y/N]",
        pendingKey: "R",
      });
      return;
    }
    if (k.sequence === "n") {
      newLog.event.dim("tip: --any to match any author, --base <ref> to branch off");
      setFooter({ kind: "input", prompt: "new:", value: "", purpose: "new" });
      return;
    }
    if (k.sequence === "N") {
      if (!current?.wt.branch) {
        toast("no branch on selected row", theme.warn, 2000);
        return;
      }
      newLog.event.info(`using ${current.wt.branch} as base`);
      setFooter({
        kind: "input",
        prompt: "new:",
        value: "",
        purpose: "new",
        base: current.wt.branch,
      });
      return;
    }
    if (isPlainLetter(k, "c")) {
      if (cleanCandidates.length === 0) {
        toast("nothing to clean", theme.fgDim, 1500);
        return;
      }
      setModal({ kind: "cleanConfirm" });
      return;
    }
    // `!` — open the action picker for the selected worktree, OR
    // open the kill-confirm if an action is currently running there.
    // Same key both ways so muscle memory stays consistent regardless
    // of state.
    if (k.sequence === "!") {
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      const slug = current.wt.slug;
      const run = actionRegistry.get(slug);
      if (run?.status === "running") {
        setModal({ kind: "killActionConfirm", slug, actionName: run.actionName });
      } else {
        openActionPicker(slug);
      }
      return;
    }
    // F10 — toggle into the selected worktree's plain shell session.
    // Persistent like F12: detach with F10, reattach to find
    // scrollback, env, and any background processes still alive.
    // `exit` / Ctrl+D ends the session.
    if (
      k.name === "f10" &&
      !k.shift &&
      !k.ctrl &&
      !k.option &&
      !k.super &&
      !k.hyper &&
      !k.meta
    ) {
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      const slug = current.wt.slug;
      if (current.status.kind === StatusKind.Busy) {
        toast(`${slug} is busy`, theme.warn, 2000);
        return;
      }
      const cwd = current.wt.path;
      const shellLog = createLogger(slug);
      void (async () => {
        shellLog.event.info("entering shell (F10 to detach)");
        const result = await enterShellSession({ renderer, slug, cwd });
        // Flip the indicator + spin up the shell-tail tailer
        // immediately rather than waiting for the 2s tmux-sessions
        // poll. Without this, lines written in the first ~2s arrive
        // only via seed-on-late-ensure, not as live deltas.
        void refreshTmuxSessions();
        if (result.kind === "spawn-failed") {
          shellLog.event.err(`shell failed to start: ${result.reason}`);
          toast(`shell failed: ${result.reason}`, theme.err, 3000);
        } else if (result.kind === "detached") {
          shellLog.event.info(`detached from shell (${slug})`);
        } else {
          shellLog.event.info(`shell exited (${result.code ?? "?"})`);
          if (result.stderr) shellLog.event.err(result.stderr);
        }
      })();
      return;
    }
    // F11 — toggle into the selected worktree's diff TUI
    // (`[diff].command`, default `gitu`). tmux's `new-session -A`
    // makes this idempotent (creates or attaches), and the
    // wt-private tmux config binds F11 to detach-client → the same
    // physical key flips between contexts. Sessions persist (named
    // `<slug>-diff`) so detach-then-reattach keeps gitu's scroll +
    // expansion state. Refuse on busy worktrees so we don't race a
    // destroy.
    if (
      k.name === "f11" &&
      !k.shift &&
      !k.ctrl &&
      !k.option &&
      !k.super &&
      !k.hyper &&
      !k.meta
    ) {
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      const slug = current.wt.slug;
      if (current.status.kind === StatusKind.Busy) {
        toast(`${slug} is busy`, theme.warn, 2000);
        return;
      }
      const cwd = current.wt.path;
      const base = resolveDiffBase(current);
      const diffLog = createLogger(slug);
      void (async () => {
        diffLog.event.info(`opening diff vs ${base} (F11 to detach)`);
        const result = await enterDiffSession({ renderer, slug, cwd, base });
        if (result.kind === "spawn-failed") {
          diffLog.event.err(`diff failed to start: ${result.reason}`);
          toast(`diff failed: ${result.reason}`, theme.err, 3000);
        } else if (result.kind === "detached") {
          diffLog.event.info(`detached from diff (${slug})`);
        } else {
          diffLog.event.info(`diff exited (${result.code ?? "?"})`);
          if (result.stderr) diffLog.event.err(result.stderr);
        }
      })();
      return;
    }
    // Shift+F10 — kill-confirm for the selected worktree's shell
    // session. Mirrors Shift+F11/F12. No-op (with a hint) when
    // there's no session. Killing terminates any background
    // processes the user launched in the shell.
    if (
      k.name === "f10" &&
      k.shift &&
      !k.ctrl &&
      !k.option &&
      !k.super &&
      !k.hyper &&
      !k.meta
    ) {
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      const slug = current.wt.slug;
      if (!activeShellSessions.has(slug)) {
        toast(`no shell session on ${slug}`, theme.fgDim, 1500);
        return;
      }
      setModal({ kind: "killSessionConfirm", slug, sessionKind: "shell" });
      return;
    }
    // Shift+F11 — kill-confirm for the selected worktree's diff
    // session. Mirrors Shift+F12. No-op (with a hint) when there's no
    // session. Killing throws away gitu's scroll/expansion state, so
    // next F11 opens fresh.
    if (
      k.name === "f11" &&
      k.shift &&
      !k.ctrl &&
      !k.option &&
      !k.super &&
      !k.hyper &&
      !k.meta
    ) {
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      const slug = current.wt.slug;
      if (!activeDiffSessions.has(slug)) {
        toast(`no diff session on ${slug}`, theme.fgDim, 1500);
        return;
      }
      setModal({ kind: "killSessionConfirm", slug, sessionKind: "diff" });
      return;
    }
    // Shift+F12 — spawn-and-attach a new auto-named claude session
    // on the selected worktree. The picker (`;`) covers the
    // explicit-name path; this is the keystroke shortcut for "give me
    // another claude here, don't make me name it". Auto-name is the
    // smallest free integer ≥ 2 in this slug's name list (primary is
    // implicit). Replaces the old kill-confirm — kill is rare enough
    // to live in the picker (`x`) only.
    if (
      k.name === "f12" &&
      k.shift &&
      !k.ctrl &&
      !k.option &&
      !k.super &&
      !k.hyper &&
      !k.meta
    ) {
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      const slug = current.wt.slug;
      if (current.status.kind === StatusKind.Busy) {
        toast(`${slug} is busy`, theme.warn, 2000);
        return;
      }
      doSpawnNamedClaudeSession(slug, nextAutoName(slug));
      return;
    }
    // F12 — toggle into the selected worktree's primary claude
    // session. tmux's `new-session -A` makes this idempotent (creates
    // or attaches), so the same key works whether the session exists
    // yet or not. From inside the session, the wt-private tmux config
    // binds F12 to detach-client → the same physical key flips
    // between contexts. Refuse on busy worktrees so we don't race a
    // destroy. The unmodified-only guard prevents Shift+F12 (handled
    // above as new-session) from also entering.
    if (
      k.name === "f12" &&
      !k.shift &&
      !k.ctrl &&
      !k.option &&
      !k.super &&
      !k.hyper &&
      !k.meta
    ) {
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      doEnterClaudeSession(current.wt.slug, null);
      return;
    }
    if (k.sequence === ",") {
      void openInZed(configFilePath);
      configLog.event.info(`opened ${configFilePath}`);
      return;
    }
    // `.` — toggle into a persistent claude session at the wt source
    // repo. Same model as F12 on a worktree: tmux's `new-session -A`
    // makes it idempotent, and F10/F11/F12 (bound to detach-client in
    // the wt-private tmux config) takes the user back out. Slug "wt"
    // so the tmux session and `/resume` entry both surface as `wt`.
    if (k.sequence === ".") {
      void (async () => {
        wtLog.event.info("entering wt claude session (F12 to detach)");
        const result = await enterClaudeSession({
          renderer,
          slug: WT_SOURCE_SLUG,
          cwd: WT_REPO_PATH,
          claudeDisplayName: WT_SOURCE_SLUG,
        });
        if (result.kind === "spawn-failed") {
          wtLog.event.err(`claude failed to start: ${result.reason}`);
          toast(`claude failed: ${result.reason}`, theme.err, 3000);
        } else if (result.kind === "detached") {
          wtLog.event.info("detached from wt claude session");
        } else {
          wtLog.event.info(`wt claude session exited (${result.code ?? "?"})`);
          if (result.stderr) wtLog.event.err(result.stderr);
        }
      })();
      return;
    }
    if (k.sequence === ">") {
      void openInZed(WT_REPO_PATH);
      wtLog.event.info(`opened ${WT_REPO_PATH}`);
      return;
    }

    // Per-row actions.
    if (!current) return;
    const rowLog = createLogger(current.wt.slug);
    if (isPlainLetter(k, "o")) {
      openInZed(current.wt.path);
      rowLog.event.info("opened in zed");
      return;
    }
    if (isPlainLetter(k, "p")) {
      if (!current.pr) {
        rowLog.event.warn("no PR for this branch");
        return;
      }
      const url = graphiteUrlFromGithubPr(current.pr.url) ?? current.pr.url;
      hideFrontmostAlacritty();
      openUrl(url);
      rowLog.event.info(`opened PR #${current.pr.number}`);
      return;
    }
    if (isPlainLetter(k, "i")) {
      const url = linearUrlForSlug(current.wt.slug);
      if (!url) {
        rowLog.event.warn("no linear id in slug");
        return;
      }
      hideFrontmostAlacritty();
      openUrl(url);
      rowLog.event.info("opened linear");
      return;
    }
    if (isPlainLetter(k, "s")) {
      if (!current.fields.deploy.data) {
        rowLog.event.warn("not deployed");
        return;
      }
      const url = stageUrl(current.wt.stage);
      if (!url) {
        rowLog.event.warn("no stage domain configured");
        return;
      }
      hideFrontmostAlacritty();
      openUrl(url);
      rowLog.event.info(`opened ${current.wt.stage}`);
      return;
    }
    if (k.sequence === "y") {
      setModal({ kind: "yank" });
      return;
    }
    if (isPlainLetter(k, "d")) {
      if (current.status.kind === StatusKind.Busy) {
        const label = current.status.op ?? current.status.label;
        toast(`${current.wt.slug} is ${label}`, theme.warn, 2000);
        return;
      }
      setFooter({
        kind: "confirm",
        message: `remove ${current.wt.slug}? [y/N]`,
        pendingKey: "d",
      });
      return;
    }
    if (isPlainLetter(k, "v")) {
      void openReviewerPicker(current.wt.slug);
      return;
    }
    if (isPlainLetter(k, "e")) {
      if (!current.pr) {
        toast("no PR for this row", theme.warn, 2000);
        return;
      }
      if (current.pr.state !== "OPEN") {
        toast("PR is not open", theme.warn, 2000);
        return;
      }
      if (!current.pr.isDraft) {
        toast("PR is already ready", theme.info, 2000);
        return;
      }
      setFooter({
        kind: "confirm",
        message: `mark #${current.pr.number} ready for review? [y/N]`,
        pendingKey: "e",
      });
      return;
    }
    if (isPlainLetter(k, "m")) {
      if (!current.pr) {
        toast("no PR for this row", theme.warn, 2000);
        return;
      }
      if (current.pr.state !== "OPEN") {
        toast("PR is not open", theme.warn, 2000);
        return;
      }
      // One-way arm: Graphite has no documented disarm path through
      // gt or its API. Use app.graphite.com to disable once armed.
      setFooter({
        kind: "confirm",
        message: `merge when ready for #${current.pr.number}? [y/N]`,
        pendingKey: "m+",
      });
      return;
    }
    if (isPlainLetter(k, "a")) {
      const slug = current.wt.slug;
      toggleArchived(slug).then(
        ({ archived }) => {
          rowLog.event.info(archived ? "archived" : "restored from archive");
          toast(archived ? `archived ${slug}` : `restored ${slug}`, theme.info, 2000);
        },
        (err) => reportActionError("archive", err),
      );
      return;
    }
    if (isPlainLetter(k, "l")) {
      openSectionPicker();
      return;
    }
    if (isPlainLetter(k, "t")) {
      if (!config.ai) {
        toast("AI summary not configured", theme.warn, 2000);
        return;
      }
      if (current.status.kind === StatusKind.Busy) {
        toast(`${current.wt.slug} is busy`, theme.warn, 2000);
        return;
      }
      const slug = current.wt.slug;
      void (async () => {
        const ok = await refreshAiSummary(slug);
        if (ok) rowLog.event.dim("regenerating AI summary");
        else toast("no diff context yet", theme.warn, 2000);
      })();
      return;
    }
  });

  // Global in-flight count — covers the root queries and the imperative
  // `fetchOriginQuery` call, not just the observed per-worktree fields.
  // Using the per-row aggregate alone made the indicator flash briefly
  // at the tail of a refresh (after `git fetch origin` resolved) instead
  // of lighting up for the whole window.
  const fetchingCount = useIsFetching();
  const activeCount = rows.filter((r) => !r.archived).length;
  const archivedCount = rows.length - activeCount;
  const titleBar = useMemo(() => {
    const suffix = isLoading
      ? " · loading..."
      : fetchingCount > 0
        ? ` · refreshing (${fetchingCount})`
        : "";
    const archivedNote = archivedCount > 0 ? ` · ${archivedCount} archived` : "";
    return ` wt · ${activeCount} worktree${activeCount === 1 ? "" : "s"}${archivedNote}${suffix} `;
  }, [activeCount, archivedCount, isLoading, fetchingCount]);

  const footerHint = useMemo(() => {
    const parts: string[] = [];
    if (filter) parts.push(`/${filter} (${filteredRows.length}/${rows.length})`);
    if (activeTails.size > 0) parts.push(`tailing ${activeTails.size}`);
    return parts.length > 0 ? parts.join(" · ") : undefined;
  }, [filter, filteredRows.length, rows.length, activeTails.size]);

  return (
    <box flexDirection="column" width={width} height={height} backgroundColor={theme.bg}>
      <box
        flexShrink={0}
        flexDirection="row"
        backgroundColor={theme.bgAlt}
        paddingLeft={1}
        paddingRight={1}
        height={1}
      >
        <box flexGrow={1} flexShrink={1} overflow="hidden">
          <text fg={theme.fgBright} attributes={1}>
            {titleBar}
          </text>
        </box>
        <ClaudeUsageBadge />
      </box>
      <box flexDirection="row" flexGrow={1}>
        <WorktreeList
          rows={filteredRows}
          selectedIndex={cursorIndex}
          width={listWidth}
          activeTails={activeTails}
          activeActions={activeActions}
          claudeSessionsBySlug={claudeSessionsBySlug}
          isLoading={isLoading}
          filter={filter}
        />
        <Details row={current} width={Math.max(0, width - listWidth)} />
      </box>
      <OutputViewer
        output={displayedOutput}
        height={activityHeight}
        pinned={
          !!focusBucket.pinned && focusBucket.pinned === displayedOutput.id
        }
      />
      {modal?.kind === "outputsPicker" ? (
        <OutputsPicker
          slug={currentSlug ?? null}
          items={visibleOutputs}
          selectedIndex={
            visibleOutputs.length === 0
              ? 0
              : Math.min(
                  Math.max(0, modal.index),
                  visibleOutputs.length - 1,
                )
          }
          pinnedId={focusBucket.pinned}
        />
      ) : null}
      {modal?.kind === "claudeSessionsPicker" ? (
        <SessionsPickerList
          slug={modal.slug}
          entries={pickerEntries}
          selectedIndex={Math.min(
            Math.max(0, modal.index),
            pickerEntries.length, // index `length` = the trailing "+ new" row
          )}
          pinnedId={(slugFocus[modal.slug] ?? EMPTY_FOCUS).pinned}
        />
      ) : null}
      {modal?.kind === "claudeSessionsNew" ? (
        <SessionsPickerNew
          slug={modal.slug}
          input={modal.input}
          autoName={nextAutoName(modal.slug)}
          error={modal.error}
        />
      ) : null}
      <Footer mode={footer} hint={footerHint} />
      {modal?.kind === "help" ? <HelpOverlay /> : null}
      {modal?.kind === "cleanConfirm" ? (
        <CleanConfirmModal candidates={cleanCandidates} />
      ) : null}
      {modal?.kind === "yank" && current ? <YankModal row={current} /> : null}
      {modal?.kind === "branchPicker" ? (
        <PickerModal
          title={modal.title}
          items={modal.items}
          selectedIndex={modal.index}
        />
      ) : null}
      {modal?.kind === "reviewerPicker" ? (
        <MultiPickerModal
          title={modal.title}
          items={modal.items}
          selectedIndex={modal.index}
          checked={modal.checked}
          toggleKey="v"
        />
      ) : null}
      {modal?.kind === "sectionPicker" ? (
        <SectionPickerModal
          title={modal.title}
          items={modal.items}
          selectedIndex={modal.index}
          newName={modal.newName}
        />
      ) : null}
      {modal?.kind === "actionPicker" && modal.state.mode === "list" ? (
        <ActionPickerModal
          slug={modal.state.slug}
          items={buildActionPickerItems(modal.state.slug)}
          selectedIndex={modal.state.index}
        />
      ) : null}
      {modal?.kind === "actionPicker" && modal.state.mode === "edit" ? (
        <ActionEditModal
          slug={modal.state.slug}
          def={modal.state.def}
          extras={modal.state.extras}
          vars={(() => {
            const row = rows.find((r) => r.wt.slug === modal.state.slug);
            return row ? buildActionVars(row) : {};
          })()}
        />
      ) : null}
      {modal?.kind === "killActionConfirm" ? (
        <KillActionConfirmModal
          slug={modal.slug}
          actionName={modal.actionName}
        />
      ) : null}
      {modal?.kind === "killSessionConfirm" ? (
        <KillSessionConfirmModal slug={modal.slug} sessionKind={modal.sessionKind} />
      ) : null}
    </box>
  );
}
