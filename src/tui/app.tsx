import { useMemo, useRef, useState } from "react";
import { useIsFetching } from "@tanstack/react-query";
import { useKeyboard, useTerminalDimensions } from "@opentui/react";

import { configFilePath } from "../core/config.ts";
import {
  createWorktree,
  parseInput,
  spawnBackgroundRemove,
} from "../core/lifecycle.ts";
import { linearUrlForSlug } from "../core/linear.ts";
import { lockLabel, lockStatus } from "../core/locks.ts";
import { createLogger } from "../core/logger.ts";
import { stageUrl } from "../core/stage.ts";
import { StatusKind } from "../core/types.ts";
import { useWtActions } from "../state/index.ts";

import { CleanConfirmModal } from "./panels/clean-confirm.tsx";
import { Details } from "./panels/details.tsx";
import { Footer, type FooterMode } from "./panels/footer.tsx";
import { HelpOverlay } from "./panels/help.tsx";
import { PickerModal } from "./panels/picker.tsx";
import { WorktreeList } from "./panels/list.tsx";
import { ActivityPane } from "./panels/activity.tsx";
import { YankModal, yankItemsFor } from "./panels/yank.tsx";
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
    clearAll,
    invalidateWorktree,
    toggleArchived,
    archive,
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
      log.event.err("refused: uncommitted changes — use `wt rm <slug> --force` from shell");
      toast(`${slug} has uncommitted changes`, theme.err, 3000);
      return;
    }
    const unpushed = row.fields.sync.data?.remote?.ahead ?? 0;
    if (unpushed > 0) {
      const plural = unpushed === 1 ? "" : "s";
      log.event.err(
        `refused: ${unpushed} unpushed commit${plural} — use \`wt rm ${slug} --force\` from shell`,
      );
      toast(`${slug} has ${unpushed} unpushed commit${plural}`, theme.err, 3000);
      return;
    }
    // Tuck the row into the archived section for the duration of the
    // destroy — keeps the active list uncluttered while tail output
    // spills into the activity pane. `clearArchived` in removeWorktree
    // drops it again once the worktree is actually gone.
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
      if (k.name === "escape" || (k.ctrl && k.name === "c")) {
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

    // Input mode: typing into the new-worktree prompt.
    if (footer.kind === "input") {
      if (k.name === "escape" || (k.ctrl && k.name === "c")) {
        setFooter({ kind: "legend" });
        return;
      }
      if (k.name === "return") {
        const raw = footer.value.trim();
        const base = footer.base;
        setFooter({ kind: "legend" });
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
    if (k.name === "q" || (k.ctrl && k.name === "c")) {
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
    if (k.name === "c") {
      if (cleanCandidates.length === 0) {
        toast("nothing to clean", theme.fgDim, 1500);
        return;
      }
      setShowCleanConfirm(true);
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
    if (k.name === "o") {
      openInZed(current.wt.path);
      rowLog.event.info("opened in zed");
      return;
    }
    if (k.name === "p") {
      if (!current.pr) {
        rowLog.event.warn("no PR for this branch");
        return;
      }
      hideFrontmostAlacritty();
      openUrl(current.pr.url);
      rowLog.event.info(`opened PR #${current.pr.number}`);
      return;
    }
    if (k.name === "i") {
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
    if (k.name === "s") {
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
    if (k.name === "d") {
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
    if (k.name === "a") {
      const slug = current.wt.slug;
      const { archived } = toggleArchived(slug);
      rowLog.event.info(archived ? "archived" : "restored from archive");
      toast(archived ? `archived ${slug}` : `restored ${slug}`, theme.info, 2000);
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
      ? " · loading…"
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
          isLoading={isLoading}
          filter={filter}
        />
        <Details row={current} />
      </box>
      <ActivityPane height={activityHeight} />
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
    </box>
  );
}
