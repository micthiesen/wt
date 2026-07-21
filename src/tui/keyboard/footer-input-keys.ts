/**
 * Footer input mode: typing into the new-worktree prompt or the
 * rename-section prompt — `purpose` discriminates which. Extracted
 * from `app.tsx`; the dispatcher calls this only while
 * `footer.kind === "input"`, and every path swallows the key.
 */
import type { KeyEvent } from "@opentui/core";

import { createLogger } from "../../core/logger.ts";
import { printableText } from "../app-helpers.ts";
import type { FooterMode } from "../panels/footer.tsx";
import { theme } from "../theme.ts";

const appLog = createLogger("[app]");

export type FooterInputKeysCtx = {
  footer: Extract<FooterMode, { kind: "input" }>;
  setFooter: (f: FooterMode) => void;
  pendingRename: string | null;
  setPendingRename: (v: string | null) => void;
  renameSection: (oldName: string, newName: string) => Promise<void>;
  setLastMoveTarget: (updater: (prev: string | null) => string | null) => void;
  toast: (message: string, color?: string, ms?: number) => void;
  doNew: (raw: string, defaultBase?: string) => Promise<void>;
  doRemoteNew: (raw: string) => Promise<void>;
};

export function handleFooterInputKey(k: KeyEvent, ctx: FooterInputKeysCtx): void {
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
  } = ctx;
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
        if (raw) {
          if (purpose === "new-remote") void doRemoteNew(raw);
          else void doNew(raw, base);
        }
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
