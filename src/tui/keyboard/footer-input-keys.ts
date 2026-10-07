/**
 * Footer input mode: typing into the new-worktree prompt or the
 * rename-section prompt — `purpose` discriminates which. Extracted
 * from `app.tsx`; the dispatcher calls this only while
 * `footer.kind === "input"`, and every path swallows the key.
 */
import type { KeyEvent } from "@opentui/core";

import { createLogger } from "../../core/logger.ts";
import { operationErrors } from "../../core/errors.ts";
import { printableText } from "../app-helpers.ts";
import { forkReported } from "../effect-boundary.ts";
import type { PendingStatusText } from "../flows/work-status.ts";
import type { makeWorktreeTitleFlows } from "../flows/worktree-title.ts";
import type { FooterMode } from "../panels/footer.tsx";
import { applyEditKey, insertText, makeEdit } from "../text-edit.tsx";
import { theme } from "../theme.ts";
import { Effect } from "effect";

const io = operationErrors("footer-input-keys");

const appLog = createLogger("[app]");

export type FooterInputKeysCtx = {
  footer: Extract<FooterMode, { kind: "input" }>;
  /**
   * React state setter (updater form included): the async failure-
   * restore path must check the CURRENT footer before overwriting —
   * a create can take seconds, and the user may have started another
   * footer interaction in the meantime.
   */
  setFooter: (f: FooterMode | ((prev: FooterMode) => FooterMode)) => void;
  pendingRename: string | null;
  setPendingRename: (v: string | null) => void;
  renameSection: (oldName: string, newName: string) => Promise<void>;
  setLastMoveTarget: (updater: (prev: string | null) => string | null) => void;
  toast: (message: string, color?: string, ms?: number) => void;
  // Both resolve to whether the create succeeded — the return, not a
  // thrown error, is the failure signal (both flows already catch their
  // own errors and toast). Used below to restore the typed line on
  // failure instead of leaving the user to retype it.
  doNew: (raw: string, defaultBase?: string) => Promise<boolean>;
  doRemoteNew: (raw: string) => Promise<boolean>;
  pendingStatusText: PendingStatusText | null;
  setPendingStatusText: (v: PendingStatusText | null) => void;
  commitStatusText: (pending: PendingStatusText, text: string) => void;
  /** Slug parked by the `#` prompt while the human types. */
  pendingIssueSlug: string | null;
  setPendingIssueSlug: (v: string | null) => void;
  commitIssueId: (slug: string, raw: string) => void;
  commitWorktreeTitle: ReturnType<typeof makeWorktreeTitleFlows>["commitWorktreeTitle"];
};

export function restoreFailedCreateFooter(
  current: FooterMode,
  submitted: Extract<FooterMode, { kind: "input" }>,
): FooterMode {
  return current.kind === "legend" ? submitted : current;
}

export function handleFooterInputKey(
  k: KeyEvent,
  ctx: FooterInputKeysCtx,
): void {
  const {
    footer,
    setFooter,
    pendingRename,
    setPendingRename,
    renameSection,
    setLastMoveTarget,
    toast,
    doNew,
    doRemoteNew,
    pendingStatusText,
    setPendingStatusText,
    commitStatusText,
    pendingIssueSlug,
    setPendingIssueSlug,
    commitIssueId,
    commitWorktreeTitle,
  } = ctx;
  if (k.name === "escape" || (k.ctrl && k.name === "c")) {
    setFooter({ kind: "legend" });
    setPendingRename(null);
    // Esc during a status pick's text entry cancels the whole
    // pick — no write.
    setPendingStatusText(null);
    setPendingIssueSlug(null);
    return;
  }
  if (k.name === "return") {
    if (footer.purpose === "worktree-title") {
      // The title flow validates before closing, so an empty submit keeps
      // the editor open and never erases the existing title.
      commitWorktreeTitle(footer);
      return;
    }
    const raw = footer.edit.value.trim();
    const base = footer.base;
    const purpose = footer.purpose;
    setFooter({ kind: "legend" });
    if (purpose === "status-text") {
      const pending = pendingStatusText;
      setPendingStatusText(null);
      // An empty line is meaningful and field-dependent —
      // `statusTextRecord` owns that decision.
      if (pending) commitStatusText(pending, raw);
      return;
    }
    if (purpose === "issue-id") {
      const slug = pendingIssueSlug;
      setPendingIssueSlug(null);
      // An empty line is meaningful here (it clears the override),
      // so it goes to the flow rather than being dropped as a
      // no-input cancel the way the create prompts treat it.
      if (slug) commitIssueId(slug, raw);
      return;
    }
    if (purpose === "rename-section") {
      const oldName = pendingRename;
      setPendingRename(null);
      if (!oldName || !raw || raw === oldName) return;
      forkReported(
        io.promise("rename section", () => renameSection(oldName, raw)).pipe(
          Effect.tap(() =>
            Effect.sync(() => {
              // Update only after persistence succeeds so a failed rename
              // cannot leave the sticky picker target pointing at fiction.
              setLastMoveTarget((prev) => (prev === oldName ? raw : prev));
              toast(`renamed "${oldName}" to "${raw}"`, theme.info, 1800);
            }),
          ),
        ),
        (error) => {
          appLog.event.err(`rename failed: ${error.message}`);
          toast(`rename failed: ${error.message}`, theme.err, 3000);
        },
      );
      return;
    }
    if (raw) {
      // Optimistic reset above already cleared the footer to legend.
      // On failure, put the typed line back in input mode (same
      // prompt/purpose/base) so a bad flag or a resolution error
      // doesn't cost the user a full retype — the flow's own toast
      // already explains why.
      // Guarded restore: the create can take seconds and the user
      // may have started ANOTHER footer interaction meanwhile — a
      // failed create must never clobber it. Only resurrect the
      // typed line if the footer is still idle.
      const restore = () =>
        setFooter((prev) =>
          restoreFailedCreateFooter(prev, {
            kind: "input",
            prompt: footer.prompt,
            edit: makeEdit(raw),
            purpose,
            base,
          }),
        );
      if (purpose === "new-remote") {
        forkReported(
          io.promise("new remote worktree", () => doRemoteNew(raw)).pipe(
            Effect.tap((ok) =>
              Effect.sync(() => {
                if (!ok) restore();
              }),
            ),
          ),
          restore,
        );
      } else {
        forkReported(
          io.promise("new worktree", () => doNew(raw, base)).pipe(
            Effect.tap((ok) =>
              Effect.sync(() => {
                if (!ok) restore();
              }),
            ),
          ),
          restore,
        );
      }
    }
    return;
  }
  // Backspace on empty input exits, matching the filter convention.
  if (k.name === "backspace" && footer.edit.value.length === 0) {
    setFooter({ kind: "legend" });
    setPendingStatusText(null);
    return;
  }
  // Cursor movement / deletion (arrows, word jumps, home/end,
  // backspace/delete) — shared editor logic.
  const edited = applyEditKey(k, footer.edit);
  if (edited) {
    setFooter({ ...footer, edit: edited });
    return;
  }
  // `k.sequence` is the literal bytes the terminal delivered — a
  // single key for typing, or a paste blob. Human titles allow Unicode;
  // other inputs retain their charset. Control characters never insert.
  const text = printableText(k.sequence, footer.purpose === "worktree-title");
  if (text) setFooter({ ...footer, edit: insertText(footer.edit, text) });
  return;
}
