/**
 * Renderer-side orchestrator for the F11 git-diff TUI. Suspends the
 * opentui renderer, hands the terminal off to the configured
 * `[diff].command` running through `core/tmux.attachOrCreate` (kind
 * `"diff"`), and resumes when the tmux client exits.
 *
 * Goes through tmux for one reason: F11 inside the diff TUI must
 * detach back to wt, which requires a layer between the user's
 * keystrokes and the inner program. tmux's wt-private config binds
 * F11 to detach-client. The session is persistent (named
 * `<slug>-diff`) so detach-then-reattach keeps your scroll/expansion
 * state, mirroring the F12 claude-session model.
 */
import type { CliRenderer } from "@opentui/core";

import { attachOrCreate, type AttachResult } from "../../core/tmux.ts";
import { handoffTerminal } from "./renderer-handoff.ts";

export type DiffResult = AttachResult;

export async function enterDiffSession(opts: {
  renderer: CliRenderer;
  slug: string;
  cwd: string;
  /**
   * Resolved diff base ref to splice into `{{base}}` in
   * `[diff].command` (default `hunk diff {{base}} --watch`). For
   * trunk-targeted worktrees this is `origin/<config.branch.base>`;
   * for stack-detected or non-trunk-PR worktrees it's the parent
   * branch ref. Forwarded verbatim to `attachOrCreate`.
   */
  base: string;
}): Promise<DiffResult> {
  const { renderer, slug, cwd, base } = opts;
  return await handoffTerminal(renderer, () =>
    attachOrCreate({ slug, cwd, kind: "diff", base }),
  );
}
