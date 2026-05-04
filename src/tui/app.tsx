import { useEffect, useMemo, useRef, useState } from "react";
import { useIsFetching } from "@tanstack/react-query";
import { useKeyboard, useTerminalDimensions } from "@opentui/react";

import { actionRegistry, type ActionDef } from "../core/actions.ts";
import { config, configFilePath } from "../core/config.ts";
import {
  createWorktree,
  parseInput,
  spawnBackgroundRemove,
} from "../core/lifecycle.ts";
import {
  disableAutoMerge,
  editReviewers,
  enableAutoMerge,
  markPullRequestReady,
} from "../core/github.ts";
import { linearUrlForSlug } from "../core/linear.ts";
import { lockLabel, lockStatus } from "../core/locks.ts";
import { createLogger } from "../core/logger.ts";
import { stageUrl } from "../core/stage.ts";
import { StatusKind } from "../core/types.ts";
import { useWtActions } from "../state/index.ts";

import {
  ActionEditModal,
  ActionPickerModal,
  type ActionPickerState,
  type PickerItem,
} from "./panels/action-picker.tsx";
import { ActionViewer } from "./panels/action-viewer.tsx";
import { CleanConfirmModal } from "./panels/clean-confirm.tsx";
import { Details } from "./panels/details.tsx";
import { Footer, type FooterMode } from "./panels/footer.tsx";
import { HelpOverlay } from "./panels/help.tsx";
import { KillActionConfirmModal } from "./panels/kill-action-confirm.tsx";
import { MultiPickerModal, PickerModal, type MultiPickerItem } from "./panels/picker.tsx";
import {
  SectionPickerModal,
  type SectionPickerItem,
} from "./panels/section-picker.tsx";
import { WorktreeList } from "./panels/list.tsx";
import { ActivityPane } from "./panels/activity.tsx";
import { YankModal, yankItemsFor } from "./panels/yank.tsx";
import { useAction, useActionVisible, useActiveActions } from "./hooks/useAction.ts";
import { useAutoCopy } from "./hooks/useAutoCopy.ts";
import { useLogTails } from "./hooks/useLogTails.ts";
import { usePaste } from "./hooks/usePaste.ts";
import { useTerminalFocus } from "./hooks/useTerminalFocus.ts";
import { useWorktreeRows, type WorktreeRow } from "./hooks/useWorktreeRows.ts";
import { hideFrontmostAlacritty, openInZed, openUrl, writeClipboard, WT_REPO_PATH } from "./helpers.ts";
import { theme } from "./theme.ts";

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

export function App({ onExit }: Props) {
  const { width, height } = useTerminalDimensions();
  const { rows, isLoading } = useWorktreeRows();
  const {
    refreshAll,
    refreshStale,
    refreshGithub,
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
  } = useWtActions();
  const [sel, setSel] = useState(0);
  const [footer, setFooter] = useState<FooterMode>({ kind: "legend" });
  const [showHelp, setShowHelp] = useState(false);
  const [showCleanConfirm, setShowCleanConfirm] = useState(false);
  const [showYank, setShowYank] = useState(false);
  const [filter, setFilter] = useState("");
  // Pending branch picker (populated when `parseInput` has multiple
  // matches for `--any`). The resolver lets the suspended `doNew`
  // promise continue once the user picks or cancels.
  const [picker, setPicker] = useState<{
    title: string;
    items: string[];
    index: number;
    resolve: (picked: string | null) => void;
  } | null>(null);
  // Multi-select picker (currently used only for "request review"). Kept
  // separate from `picker` because it doesn't suspend a caller — it just
  // dispatches an action on submit.
  const [reviewerPicker, setReviewerPicker] = useState<{
    title: string;
    items: MultiPickerItem[];
    index: number;
    checked: Set<string>;
    /** Snapshot of currently-requested logins; diffed on submit. */
    original: Set<string>;
    slug: string;
    prNumber: number;
  } | null>(null);
  // Section picker (mapped to `l`). When `newName !== null` the modal
  // is in "+ new section" input mode — the keypress handler sends
  // typing into that string instead of navigating the list.
  const [sectionPicker, setSectionPicker] = useState<{
    title: string;
    slug: string;
    items: SectionPickerItem[];
    index: number;
    newName: string | null;
  } | null>(null);
  // Cursor-follow ref. After every move/archive/rename, we want the
  // cursor to land on the same slug at its new display index. The
  // tricky part is that `await action()` resolves once the query
  // refetch settles, but React hasn't re-rendered yet — so reading
  // the cursor position immediately would land on the *pre-move*
  // index. Instead we stash {slug, rowsAtDispatch} in a ref and let
  // a `useEffect` on `filteredRows` consume it once the rows
  // reference has actually changed (which happens on the
  // post-invalidation render). The `rowsAtDispatch` comparison
  // is the gate that prevents a stale-data render from snapping the
  // cursor to the old position. Ref over state because we don't want
  // an extra render just to schedule the follow.
  const followRef = useRef<{
    slug: string;
    rowsAtDispatch: WorktreeRow[];
  } | null>(null);
  // Last section the user moved a row into. Used to default the
  // section-picker cursor — the common case is "moving several
  // adjacent worktrees into the same section", and re-aiming on
  // every open eats keystrokes. Reset to `null` on rename so the
  // sticky target doesn't dangle.
  const [lastMoveTarget, setLastMoveTarget] = useState<string | null>(null);
  const [actionPicker, setActionPicker] = useState<ActionPickerState | null>(null);
  // Modal rather than inline `footer.confirm`: killing live work is
  // intentionally heavier than the y/N row on the footer.
  const [killActionConfirm, setKillActionConfirm] = useState<{
    slug: string;
    actionName: string;
  } | null>(null);
  // Section the user is renaming, if any. The footer input prompt is
  // generic; this lets the submit handler know which section the
  // typed text applies to.
  const [pendingRename, setPendingRename] = useState<string | null>(null);
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
    if (actionPicker?.mode === "edit") {
      const clean = printableMultiline(text);
      if (!clean) return;
      setActionPicker({ ...actionPicker, extras: actionPicker.extras + clean });
      return;
    }
    const clean = printableText(text);
    if (!clean) return;
    if (footer.kind === "filter") {
      const next = footer.value + clean;
      setFooter({ kind: "filter", value: next });
      setFilter(next);
      setSel(0);
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

  const effectiveSel = Math.max(0, Math.min(sel, filteredRows.length - 1));
  const current = filteredRows[effectiveSel];

  const listWidth = Math.max(32, Math.min(52, Math.floor(width * 0.44)));
  const activityHeight = Math.max(6, Math.min(16, Math.floor(height * 0.35)));

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
   * Mark a slug for cursor-follow on the next post-invalidation
   * render. Captures the *current* `filteredRows` so the effect can
   * tell when fresh data has arrived: if the rows reference is still
   * `rowsAtDispatch` when the effect runs, the refetch hasn't landed
   * and we wait. Repeated calls (chord-Shift+J) overwrite the ref
   * with a fresh `rowsAtDispatch` snapshot, so the second move's
   * effect won't consume a stale capture from the first.
   */
  function followAfterMove(slug: string): void {
    followRef.current = { slug, rowsAtDispatch: filteredRows };
  }

  /**
   * Standard error reporter for state-mutation chains. Disk writes
   * inside `wtstate.ts` can throw on EACCES / ENOSPC; we surface as
   * an event log line + toast and clear any pending follow so a
   * failed action doesn't leave the cursor logic stuck.
   */
  function reportActionError(label: string, err: unknown): void {
    followRef.current = null;
    const msg = err instanceof Error ? err.message : String(err);
    appLog.event.err(`${label} failed: ${msg}`);
    toast(`${label} failed: ${msg}`, theme.err, 3000);
  }

  // Run after every render where `filteredRows` changed. If a follow
  // is pending and the rows reference has moved off the dispatch
  // snapshot, find the slug at its new position and snap the cursor.
  // `setSel` triggers another render, but that render will see
  // `followRef.current === null` and skip.
  useEffect(() => {
    const pending = followRef.current;
    if (!pending) return;
    if (filteredRows === pending.rowsAtDispatch) return;
    const idx = filteredRows.findIndex((r) => r.wt.slug === pending.slug);
    if (idx >= 0) setSel(idx);
    followRef.current = null;
  }, [filteredRows]);

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
        followAfterMove(slug);
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
      followAfterMove(slug);
      swapOrder(slug, target.wt.slug, current.section, bucket).catch((err) =>
        reportActionError("reorder", err),
      );
      return;
    }
    // Cross-section: place at the edge of `target.section` adjacent to
    // the source section, so the row shifts one visual position.
    const position: "top" | "bottom" = dir > 0 ? "top" : "bottom";
    followAfterMove(slug);
    placeSlug(slug, target.section, position).then(
      () => {
        const label = target.section === null ? "(none)" : target.section;
        toast(`moved to ${label}`, theme.info, 1200);
      },
      (err) => reportActionError("move", err),
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
    setSectionPicker({
      title: `move ${current.wt.slug} to section`,
      slug: current.wt.slug,
      items,
      index: initial,
      newName: null,
    });
  }

  function commitSectionPick(item: SectionPickerItem, slug: string): void {
    if (item.kind === "none") {
      followAfterMove(slug);
      setSection(slug, null).then(
        () => toast("moved to (none)", theme.info, 1500),
        (err) => reportActionError("move", err),
      );
      setLastMoveTarget(null);
      setSectionPicker(null);
      return;
    }
    if (item.kind === "section") {
      const target = item.name;
      followAfterMove(slug);
      setSection(slug, target).then(
        () => toast(`moved to ${target}`, theme.info, 1500),
        (err) => reportActionError("move", err),
      );
      setLastMoveTarget(target);
      setSectionPicker(null);
      return;
    }
    // "+ new section" — switch to input mode. Submission lives in the
    // keyboard handler.
    setSectionPicker((p) => (p ? { ...p, newName: "" } : null));
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
    if (row.fields.dirty.data) {
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
    for (const row of candidates) {
      archive(row.wt.slug);
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
    setReviewerPicker({
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
    if (!reviewerPicker) return;
    const { slug, prNumber, checked, original } = reviewerPicker;
    const log = createLogger(slug);
    const add: string[] = [];
    const remove: string[] = [];
    for (const k of checked) if (!original.has(k)) add.push(k);
    for (const k of original) if (!checked.has(k)) remove.push(k);
    setReviewerPicker(null);
    if (add.length === 0 && remove.length === 0) {
      toast("no changes", theme.fgDim, 1500);
      return;
    }
    const result = await editReviewers(prNumber, { add, remove });
    if (!result.ok) {
      log.event.err(`edit reviewers failed for #${prNumber}: ${result.error}`);
      toast(`edit reviewers failed: ${result.error}`, theme.err, 4000);
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
    void refreshGithub();
  }

  async function doMarkReady(slug: string): Promise<void> {
    const log = createLogger(slug);
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row?.pr) {
      toast("no PR for this row", theme.warn, 2000);
      return;
    }
    const prNumber = row.pr.number;
    const result = await markPullRequestReady(prNumber);
    if (!result.ok) {
      log.event.err(`mark ready failed for #${prNumber}: ${result.error}`);
      toast(`mark ready failed: ${result.error}`, theme.err, 4000);
      return;
    }
    log.event.ok(`marked #${prNumber} ready for review`);
    toast(`marked #${prNumber} ready`, theme.ok, 2500);
    void refreshGithub();
  }

  async function doAutoMerge(slug: string, action: "enable" | "disable"): Promise<void> {
    const log = createLogger(slug);
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row?.pr) {
      toast("no PR for this row", theme.warn, 2000);
      return;
    }
    const prNumber = row.pr.number;
    const result =
      action === "enable"
        ? await enableAutoMerge(prNumber)
        : await disableAutoMerge(prNumber);
    if (!result.ok) {
      const verb = action === "enable" ? "auto-merge" : "disable auto-merge";
      log.event.err(`${verb} failed for #${prNumber}: ${result.error}`);
      toast(`${verb} failed: ${result.error}`, theme.err, 4000);
      return;
    }
    const past = action === "enable" ? "enabled" : "disabled";
    log.event.ok(`auto-merge ${past} for #${prNumber}`);
    toast(`auto-merge ${past} for #${prNumber}`, theme.ok, 2500);
    void refreshGithub();
  }

  function buildActionPickerItems(): PickerItem[] {
    return [
      ...config.actions.map((def) => ({ kind: "action" as const, def })),
      { kind: "custom" as const },
    ];
  }

  function openActionPicker(slug: string): void {
    const items = buildActionPickerItems();
    setActionPicker({ mode: "list", slug, index: 0, items });
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
    const result = def
      ? actionRegistry.start(def, slug, row.wt.path, extras)
      : actionRegistry.startCustom(slug, row.wt.path, extras);
    if (!result.ok) {
      toast(`action: ${result.reason}`, theme.err, 3000);
      return;
    }
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
            setPicker({
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
    openInZed(result.path);
    void refreshAll();
  }

  useKeyboard((k) => {
    // Help overlay swallows input while open.
    if (showHelp) {
      if (
        k.name === "escape" ||
        k.sequence === "?" ||
        k.name === "q" ||
        (k.ctrl && k.name === "c")
      ) {
        setShowHelp(false);
      }
      return;
    }

    // Reviewer multi-picker. Space toggles the cursor item, enter
    // submits the checked set, esc cancels.
    if (reviewerPicker) {
      if (k.name === "j" || k.name === "down") {
        setReviewerPicker({
          ...reviewerPicker,
          index: Math.min(
            reviewerPicker.index + 1,
            reviewerPicker.items.length - 1,
          ),
        });
        return;
      }
      if (k.name === "k" || k.name === "up") {
        setReviewerPicker({
          ...reviewerPicker,
          index: Math.max(reviewerPicker.index - 1, 0),
        });
        return;
      }
      if (k.name === "space" || k.sequence === " ") {
        const item = reviewerPicker.items[reviewerPicker.index];
        if (item) {
          const next = new Set(reviewerPicker.checked);
          if (next.has(item.key)) next.delete(item.key);
          else next.add(item.key);
          setReviewerPicker({ ...reviewerPicker, checked: next });
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
        setReviewerPicker(null);
      }
      return;
    }

    // Section picker (`l`). Two modes: list mode for picking an
    // existing section / "(none)" / "+ new section", and input mode
    // for typing the new section name when the user picks the create
    // entry. `newName === null` means list mode.
    if (sectionPicker) {
      const sp = sectionPicker;
      if (sp.newName !== null) {
        if (k.name === "escape") {
          setSectionPicker({ ...sp, newName: null });
          return;
        }
        if (k.ctrl && k.name === "c") {
          setSectionPicker(null);
          return;
        }
        if (k.name === "return") {
          const name = sp.newName.trim();
          if (!name) {
            setSectionPicker({ ...sp, newName: null });
            return;
          }
          const slug = sp.slug;
          followAfterMove(slug);
          setSection(slug, name).then(
            () => toast(`moved to ${name}`, theme.info, 1500),
            (err) => reportActionError("move", err),
          );
          setLastMoveTarget(name);
          setSectionPicker(null);
          return;
        }
        if (k.name === "backspace") {
          // Backspace on empty input pops back to list mode — matches
          // the filter / new-worktree input convention.
          if (sp.newName.length === 0) {
            setSectionPicker({ ...sp, newName: null });
            return;
          }
          setSectionPicker({ ...sp, newName: sp.newName.slice(0, -1) });
          return;
        }
        const text = printableText(k.sequence);
        if (text) setSectionPicker({ ...sp, newName: sp.newName + text });
        return;
      }
      if (k.name === "j" || k.name === "down") {
        setSectionPicker({
          ...sp,
          index: Math.min(sp.index + 1, sp.items.length - 1),
        });
        return;
      }
      if (k.name === "k" || k.name === "up") {
        setSectionPicker({ ...sp, index: Math.max(sp.index - 1, 0) });
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
        setSectionPicker(null);
      }
      return;
    }

    // Action picker — list of pre-built actions plus a trailing
    // "Custom prompt..." entry. Two screens: list mode (j/k + return),
    // then edit mode where the user types extras / a freeform prompt.
    if (actionPicker) {
      const ap = actionPicker;
      if (ap.mode === "list") {
        if (k.name === "j" || k.name === "down") {
          setActionPicker({
            ...ap,
            index: Math.min(ap.index + 1, ap.items.length - 1),
          });
          return;
        }
        if (k.name === "k" || k.name === "up") {
          setActionPicker({ ...ap, index: Math.max(ap.index - 1, 0) });
          return;
        }
        // `!` chord — jumps straight to the custom-prompt entry. Mirrors
        // the section picker's `l → "+ new section"` chord, so `! !`
        // from normal mode lands directly in freeform-edit mode.
        if (k.sequence === "!") {
          setActionPicker({
            mode: "edit",
            slug: ap.slug,
            def: null,
            extras: "",
          });
          return;
        }
        // Quick-pick digits 1..9 jump straight to that *action* item
        // (the custom entry is reachable via `!`, not a digit). Out-of-
        // range digits are ignored so a stray "9" in a list of three
        // doesn't fire something unintended.
        if (k.sequence && /^[1-9]$/.test(k.sequence)) {
          const i = parseInt(k.sequence, 10) - 1;
          const item = ap.items[i];
          if (item && item.kind === "action") {
            setActionPicker({
              mode: "edit",
              slug: ap.slug,
              def: item.def,
              extras: "",
            });
          }
          return;
        }
        if (k.name === "return") {
          const item = ap.items[ap.index];
          if (!item) return;
          setActionPicker({
            mode: "edit",
            slug: ap.slug,
            def: item.kind === "action" ? item.def : null,
            extras: "",
          });
          return;
        }
        if (
          k.name === "escape" ||
          k.sequence === "q" ||
          (k.ctrl && k.name === "c")
        ) {
          setActionPicker(null);
        }
        return;
      }
      // mode === "edit"
      if (k.ctrl && k.name === "c") {
        setActionPicker(null);
        return;
      }
      if (k.name === "escape") {
        // Pre-built path: pop back to the list at the same item the user
        // selected. Custom path: there's no informative list state to
        // restore (custom is the only entry there), so esc cancels out.
        const def = ap.def;
        if (def) {
          const items = buildActionPickerItems();
          const idx = items.findIndex(
            (it) => it.kind === "action" && it.def.id === def.id,
          );
          setActionPicker({
            mode: "list",
            slug: ap.slug,
            index: Math.max(0, idx),
            items,
          });
        } else {
          setActionPicker(null);
        }
        return;
      }
      if (k.name === "return") {
        const { slug, def, extras } = ap;
        setActionPicker(null);
        launchAction(slug, def, extras);
        return;
      }
      if (k.name === "backspace") {
        if (ap.extras.length === 0) return;
        setActionPicker({ ...ap, extras: ap.extras.slice(0, -1) });
        return;
      }
      const text = printableMultiline(k.sequence);
      if (text) setActionPicker({ ...ap, extras: ap.extras + text });
      return;
    }

    // Kill-confirm for an in-flight `!` action on the selected
    // worktree. Mirrors the y/N pattern used by clean-confirm; also
    // accepts `!` (the opening key) per the modal toggle convention.
    if (killActionConfirm) {
      if (k.name === "y" || k.name === "return") {
        const { slug, actionName } = killActionConfirm;
        setKillActionConfirm(null);
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
        setKillActionConfirm(null);
      }
      return;
    }

    // Branch-picker modal (for --any multi-match). Swallows input
    // until the user picks or cancels, resolving the promise that
    // `parseInput` is awaiting inside `doNew`.
    if (picker) {
      if (k.name === "j" || k.name === "down") {
        setPicker({
          ...picker,
          index: Math.min(picker.index + 1, picker.items.length - 1),
        });
        return;
      }
      if (k.name === "k" || k.name === "up") {
        setPicker({ ...picker, index: Math.max(picker.index - 1, 0) });
        return;
      }
      if (k.name === "return") {
        const chosen = picker.items[picker.index]!;
        picker.resolve(chosen);
        setPicker(null);
        return;
      }
      if (
        k.name === "escape" ||
        k.sequence === "q" ||
        (k.ctrl && k.name === "c")
      ) {
        picker.resolve(null);
        setPicker(null);
      }
      return;
    }

    // Yank chord: `y` opened the menu; the next key picks what to copy.
    // `y` again, esc, or ctrl+c cancels. Unmapped keys are ignored
    // rather than re-entering normal mode, so a stray keystroke can't
    // accidentally trigger a destructive action.
    if (showYank) {
      if (
        k.name === "escape" ||
        k.sequence === "y" ||
        k.sequence === "q" ||
        (k.ctrl && k.name === "c")
      ) {
        setShowYank(false);
        return;
      }
      if (current) {
        const item = yankItemsFor(current).find((it) => it.key === k.sequence);
        if (item) {
          setShowYank(false);
          doYank(current.wt.slug, item.label, item.value);
        }
      }
      return;
    }

    // Clean-confirm modal swallows input while open.
    if (showCleanConfirm) {
      if (k.name === "y" || k.name === "return") {
        setShowCleanConfirm(false);
        void doClean();
        return;
      }
      if (
        k.name === "n" ||
        k.name === "escape" ||
        k.sequence === "q" ||
        (k.ctrl && k.name === "c")
      ) {
        setShowCleanConfirm(false);
      }
      return;
    }

    // Filter mode: typing live-narrows the list.
    if (footer.kind === "filter") {
      if (k.name === "escape" || (k.ctrl && k.name === "c")) {
        setFilter("");
        setFooter({ kind: "legend" });
        setSel(0);
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
          setSel(0);
          return;
        }
        const next = footer.value.slice(0, -1);
        setFooter({ kind: "filter", value: next });
        setFilter(next);
        setSel(0);
        return;
      }
      const text = printableText(k.sequence);
      if (text) {
        const next = footer.value + text;
        setFooter({ kind: "filter", value: next });
        setFilter(next);
        setSel(0);
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
          // Capture the slug now — `current` is a closure over this
          // render and the user could navigate away before the
          // promise resolves; we want the cursor to track the
          // worktree whose section we just renamed.
          const slug = current?.wt.slug;
          renameSection(oldName, raw).then(
            () => { if (slug) followAfterMove(slug); },
            (err) => {
              const msg = err instanceof Error ? err.message : String(err);
              appLog.event.err(`rename failed: ${msg}`);
              toast(`rename failed: ${msg}`, theme.err, 3000);
            },
          );
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
          void doAutoMerge(current.wt.slug, "enable");
        } else if (pending === "m-" && current) {
          void doAutoMerge(current.wt.slug, "disable");
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
      setSel(0);
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
    if (k.name === "j" || k.name === "down") {
      setSel((i) => Math.min(i + 1, Math.max(0, filteredRows.length - 1)));
      return;
    }
    if (k.name === "k" || k.name === "up") {
      setSel((i) => Math.max(0, i - 1));
      return;
    }
    // The raw-stdin keypress parser lowercases `name` for A–Z and sets
    // `shift: true`, so case-sensitive bindings (`g`/`G`, `r`/`R`) have
    // to disambiguate on `sequence` rather than `name`.
    if (k.sequence === "g") {
      setSel(0);
      return;
    }
    if (k.sequence === "G") {
      setSel(Math.max(0, filteredRows.length - 1));
      return;
    }
    if (isPlainLetter(k, "q") || (k.ctrl && k.name === "c")) {
      quit();
      return;
    }
    if (k.sequence === "?") {
      setShowHelp(true);
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
      setShowCleanConfirm(true);
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
        setKillActionConfirm({ slug, actionName: run.actionName });
      } else {
        openActionPicker(slug);
      }
      return;
    }
    if (k.sequence === ",") {
      void openInZed(configFilePath);
      configLog.event.info(`opened ${configFilePath}`);
      return;
    }
    if (k.sequence === ".") {
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
      hideFrontmostAlacritty();
      openUrl(current.pr.url);
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
      setShowYank(true);
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
      // Toggle: if already armed, the same key prompts to disable.
      if (current.pr.autoMerge) {
        setFooter({
          kind: "confirm",
          message: `disable auto-merge for #${current.pr.number}? [y/N]`,
          pendingKey: "m-",
        });
        return;
      }
      setFooter({
        kind: "confirm",
        message: `merge when ready for #${current.pr.number}? [y/N]`,
        pendingKey: "m+",
      });
      return;
    }
    if (isPlainLetter(k, "a")) {
      const slug = current.wt.slug;
      followAfterMove(slug);
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
        backgroundColor={theme.bgAlt}
        paddingLeft={1}
        paddingRight={1}
        height={1}
      >
        <text fg={theme.fgBright} attributes={1}>
          {titleBar}
        </text>
      </box>
      <box flexDirection="row" flexGrow={1}>
        <WorktreeList
          rows={filteredRows}
          selectedIndex={effectiveSel}
          width={listWidth}
          activeTails={activeTails}
          activeActions={activeActions}
          isLoading={isLoading}
          filter={filter}
        />
        <Details row={current} width={Math.max(0, width - listWidth)} />
      </box>
      {showActionViewer && currentRun ? (
        <ActionViewer run={currentRun} height={activityHeight} />
      ) : (
        <ActivityPane height={activityHeight} />
      )}
      <Footer mode={footer} hint={footerHint} />
      {showHelp ? <HelpOverlay /> : null}
      {showCleanConfirm ? <CleanConfirmModal candidates={cleanCandidates} /> : null}
      {showYank && current ? <YankModal row={current} /> : null}
      {picker ? (
        <PickerModal
          title={picker.title}
          items={picker.items}
          selectedIndex={picker.index}
        />
      ) : null}
      {reviewerPicker ? (
        <MultiPickerModal
          title={reviewerPicker.title}
          items={reviewerPicker.items}
          selectedIndex={reviewerPicker.index}
          checked={reviewerPicker.checked}
          toggleKey="v"
        />
      ) : null}
      {sectionPicker ? (
        <SectionPickerModal
          title={sectionPicker.title}
          items={sectionPicker.items}
          selectedIndex={sectionPicker.index}
          newName={sectionPicker.newName}
        />
      ) : null}
      {actionPicker?.mode === "list" ? (
        <ActionPickerModal
          slug={actionPicker.slug}
          items={actionPicker.items}
          selectedIndex={actionPicker.index}
        />
      ) : null}
      {actionPicker?.mode === "edit" ? (
        <ActionEditModal
          slug={actionPicker.slug}
          def={actionPicker.def}
          extras={actionPicker.extras}
        />
      ) : null}
      {killActionConfirm ? (
        <KillActionConfirmModal
          slug={killActionConfirm.slug}
          actionName={killActionConfirm.actionName}
        />
      ) : null}
    </box>
  );
}
