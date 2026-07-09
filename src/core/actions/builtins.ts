import type { ActionDef } from "../config.ts";

/**
 * Built-in actions appended after `config.actions` in the picker. They
 * behave exactly like user-configured actions — the `actionRegistry`
 * doesn't distinguish, the only difference is they're defined in code
 * rather than read from `config.toml`. Adding one: declare it here, no
 * other wiring needed; the picker places these between user actions and
 * the trailing "Custom prompt…" sentinel.
 *
 * Intentionally EMPTY: every built-in candidate so far has been
 * project-specific (e.g. the old `pnpm sst remove` "Remove local"), which
 * belongs in the user's `config.toml`, not baked into the OSS app — the
 * same "no client-app defaults in code" rule the config loader enforces.
 * Keep this for a genuinely repo-agnostic action if one ever earns it.
 */
export const BUILTIN_ACTIONS: readonly ActionDef[] = [];

/**
 * Window during which a finished run keeps auto-focusing the bottom
 * pane in "follow selected row" mode. Exported so `useActionVisible`
 * reuses the same constant — it drives both the registry-side
 * `isVisible` predicate and the client-side timer, and they have to
 * stay in lockstep. After this window the run drops out of auto-
 * focus, but stays in memory (`MAX_RETAINED_RUNS`) so the Outputs
 * picker can still surface it as a "done"/"failed"/"killed" entry.
 */
export const RECENT_WINDOW_MS = 10 * 1000;
/**
 * How many completed runs to keep in the in-memory registry before
 * evicting the oldest. Drives the Outputs picker — bigger means more
 * historical runs visible without restart, but more memory held. The
 * boot reconciler also uses this cap when rehydrating from disk.
 */
export const MAX_RETAINED_RUNS = 20;
/** actionId stamped on runs launched via the picker's "Custom prompt…" entry. */
export const CUSTOM_ACTION_ID = "__custom__";
