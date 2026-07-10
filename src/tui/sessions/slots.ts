/**
 * Session slots — non-worktree projects we host an AI harness session
 * for. Three instances today: the wt source repo itself (`,` keybind),
 * the configured `paths.main_clone` (`.` keybind), and the user's
 * dotfiles (`/` keybind).
 *
 * Slots reuse all the harness / tmux / session-tail machinery a
 * worktree row uses. They differ in that they don't appear in the
 * list panel, don't carry a PR, and don't participate in any per-
 * worktree state queries.
 *
 * Slot slugs share the namespace with worktree slugs in tmux, the
 * session-tail registry, the orphan reaper, and claude's /resume
 * listings. The current values (`"wt"`, `"main"`) are short and can
 * in theory collide with a user-created worktree whose directory
 * basename happens to match — vanishingly unlikely in practice, but
 * the `pathBySlug` build in `app.tsx`'s session-tail reconcile is
 * ordered so a real row's path wins on tie.
 *
 * Consumers:
 *  - `tui/app.tsx` — `,` / `.` / `/` keybind handlers enter the slot
 *    via `enterHarnessSession` with the slot's path as `cwd`, picking
 *    the Shift+TAB-cycled primary harness so the choice mirrors a row's
 *    F12 default.
 *  - `tui/runtime.tsx` — the startup orphan reaper whitelists
 *    `SLOT_SLUGS` so slot-owned tmux sessions survive the per-slug
 *    cleanup sweep.
 *  - `tui/panels/footer.tsx` — subscribes to `MAIN_CLONE_SLOT`'s tail
 *    via `useSessionRun` and renders the last line in the bottom bar.
 *  - `tui/app.tsx`'s session-tail reconcile effect — adds slot paths
 *    to `pathBySlug` so a slot's live claude session gets a tailer.
 */
import { homedir } from "node:os";
import { join, resolve } from "node:path";

import { config } from "../../core/config.ts";
import { WT_SOURCE_SLUG } from "../../core/tmux.ts";

/**
 * Path of the wt source tree itself. This file lives at
 * `<repo>/src/tui/sessions/slots.ts`, so the repo root is three levels
 * up from `import.meta.dir`. Resolves consistently whether wt is
 * invoked through the bin shim or directly via `bun src/main.ts`.
 */
const WT_REPO_PATH: string = resolve(import.meta.dir, "..", "..", "..");

export type SessionSlot = {
  /** Tmux slug. Shares the namespace with worktree slugs. */
  slug: string;
  /** cwd for the harness session — also feeds claude's project-dir
   *  derivation, so the jsonl lands in a stable per-slot location. */
  path: string;
  /** Human label. Surfaces in event-log lines and the bottom-bar
   *  prefix; also passed as claude's `claudeDisplayName` so the slot's
   *  /resume entry shows the same word. */
  label: string;
};

/**
 * Slot for the wt source repo itself. Backs the `,` keybind. Slug is
 * the historical `"wt"` value so anyone with an existing wt-source
 * claude conversation keeps it across this refactor.
 */
export const WT_SOURCE_SLOT: SessionSlot = {
  slug: WT_SOURCE_SLUG,
  path: WT_REPO_PATH,
  label: "wt",
};

/**
 * Slot for the user's configured main clone. Backs the `.` keybind
 * and feeds the bottom-bar tail. Slug is the fixed string `"main"`
 * rather than something derived from `config.paths.mainClone`, so
 * the tmux session name (and claude /resume entry) stay stable
 * across machines with different `mainClone` values.
 */
export const MAIN_CLONE_SLOT: SessionSlot = {
  slug: "main",
  path: config.paths.mainClone,
  label: "main",
};

/**
 * Slot for the user's dotfiles. Backs the `/` keybind. General-purpose
 * config-editing session; `~/.dotfiles` is the actual git repo, so the
 * slot's harness lands in a versioned tree (cwd doesn't fence the
 * harness in — it can still touch `~/.config/...` by absolute path).
 */
export const DOTFILES_SLOT: SessionSlot = {
  slug: "dotfiles",
  path: join(homedir(), ".dotfiles"),
  label: "dotfiles",
};

/**
 * Every registered slot in display order. Iterated by the session-
 * tail reconcile (to map slot slugs to paths) and the orphan reaper
 * (to whitelist slot slugs).
 */
export const SESSION_SLOTS: readonly SessionSlot[] = [
  WT_SOURCE_SLOT,
  MAIN_CLONE_SLOT,
  DOTFILES_SLOT,
];

/** Convenience projection — just the slugs, for set membership tests. */
export const SLOT_SLUGS: readonly string[] = SESSION_SLOTS.map((s) => s.slug);
