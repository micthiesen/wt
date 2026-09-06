#!/usr/bin/env bun

import { Cause, Effect } from "effect";

import { OperationError, operationErrors } from "./core/errors.ts";

// Make `Bun.stringWidth` treat East-Asian-Ambiguous codepoints as 2-cell
// before any opentui code loads. Our patched Lilex Nerd Font sets the
// advance for every PUA icon to 2 mono cells; opentui's text layout
// calls `Bun.stringWidth` with default options (which counts PUA as 1)
// and ends up shoving subsequent text into the icon's right half. The
// override aligns opentui's count with what the terminal actually
// renders, so spans, columns, and right-pinned clusters line up.
const _origStringWidth = Bun.stringWidth;
Bun.stringWidth = ((s: string, opts?: Bun.StringWidthOptions) =>
  _origStringWidth(s, { ...(opts ?? {}), ambiguousIsNarrow: false })) as typeof Bun.stringWidth;

const io = operationErrors("wt");

/**
 * Load one command module and run it. The self-update family routes
 * AROUND `cli/index.ts` even though that module loads commands lazily:
 * these three must work when a bad update broke anything else, and the
 * dispatcher itself is one more module that can fail to parse
 * (`wt rollback` is the documented recovery path then) — so this stays a
 * local helper rather than an import of the dispatcher's own.
 */
function runCommand(
  name: string,
  load: () => Promise<{ run: (argv: string[]) => Effect.Effect<number, Error> }>,
  argv: string[],
): Effect.Effect<number, OperationError> {
  return io.promise(`load ${name} command`, load).pipe(
    Effect.flatMap(({ run }) => Effect.suspend(() => run(argv))),
    Effect.mapError((cause) => cause instanceof OperationError ? cause : io.wrap(`run ${name} command`)(cause)),
  );
}

/**
 * What the user sees when the program fails. A tagged failure is an
 * expected error whose `message` already reads `operation: cause` down
 * the chain, so print that; a defect (an untagged throw) keeps its stack,
 * because the stack is the only clue it carries. `WT_DEBUG` adds the
 * stack to expected failures too.
 */
function renderFailure(cause: Cause.Cause<unknown>): string {
  const error = Cause.squash(cause);
  if (error instanceof Error && "_tag" in error) {
    const text = error.message || String(error);
    return process.env.WT_DEBUG && error.stack ? `${text}\n${error.stack}` : text;
  }
  return error instanceof Error ? error.stack ?? error.message : String(error);
}

const main = Effect.fn("wt")(function* () {
  const argv = process.argv.slice(2);
    // Args given → dispatch to CLI. The self-update family routes AROUND
    // cli/index.ts even though that module now imports commands lazily:
    // these three must work when a bad update broke *anything* else, and
    // the dispatcher itself is one more module that can fail to parse
    // (`wt rollback` is the documented recovery path then).
    if (argv.length > 0) {
      const [cmd, ...rest] = argv;
      if (cmd === "update") {
        return yield* runCommand("update", () => import("./cli/commands/update.ts"), rest);
      }
      if (cmd === "rollback") {
        return yield* runCommand("rollback", () => import("./cli/commands/rollback.ts"), rest);
      }
      if (cmd === "version" || cmd === "--version" || cmd === "-v") {
        return yield* runCommand("version", () => import("./cli/commands/version.ts"), rest);
      }
      const { dispatch } = yield* io.promise("load CLI dispatcher", () => import("./cli/index.ts"));
      return yield* dispatch(argv);
    }

    // No args + non-TTY → fall back to `ls` (matches the old Python tool's
    // behavior for piped/scripted use).
    if (!process.stdout.isTTY) {
      const { dispatch } = yield* io.promise("load CLI dispatcher", () => import("./cli/index.ts"));
      return yield* dispatch(["ls"]);
    }

    // No args + TTY → interactive TUI. Every user action runs in-TUI now
    // (no CLI handoff for `new` or `clean`), so this is a single call.
    const { config } = yield* io.promise("load config", () => import("./core/config.ts"));
    if (config.instance.role === "worker") {
      yield* Effect.sync(() => console.error("wt TUI is disabled in worker mode; launch it on the controller"));
      return 2;
    }
  // Skills/instructions freshness check BEFORE the TUI takes the
  // terminal, so accepted updates are live for every agent session
  // spawned from this run. Silent when nothing is pending.
  // WT_SKILLS=off is the per-run kill switch (probe harness arms it —
  // a probe stuck on this y/n has no safe answer: both are remembered
  // in machine-global skills memory and would consume the human's own
  // prompt for that version).
    if (config.skills.startupCheck && process.env.WT_SKILLS !== "off") {
      const { startupSkillsPrompt } = yield* io.promise("load skills startup check", () => import("./cli/skills-sync.ts"));
      yield* startupSkillsPrompt().pipe(
        Effect.mapError(io.wrap("run skills startup check")),
      );
    }
  // Self-update check, after skills (both prompt on this terminal).
  // An accepted pull re-execs a fresh process instead of continuing:
  // main.ts/config.ts are already loaded from the old code, and lazy
  // TUI imports would come from the new checkout — never run the mix.
  // WT_UPDATE=off is the per-run kill switch (probe harness arms it).
    if (config.update.startupCheck && process.env.WT_UPDATE !== "off") {
      const { startupUpdatePrompt } = yield* io.promise("load update startup check", () => import("./cli/commands/update.ts"));
      if ((yield* startupUpdatePrompt().pipe(
        Effect.mapError(io.wrap("run update startup check")),
      )) === "updated") {
        const { spawnFreshWt } = yield* io.promise("load fresh wt launcher", () => import("./core/update.ts"));
        return yield* Effect.sync(spawnFreshWt);
      }
    }
  // Reconcile from the freshly loaded build on every TUI startup. Doing this
  // only inside the updater misses the first upgrade from a version that
  // predates the restart hook, and misses source changes applied elsewhere.
  // Installed + stale (including an unstamped legacy daemon) restarts; current
  // daemons are untouched. A daemon failure must not prevent the TUI booting.
    if (process.env.WT_UPDATE !== "off") {
      const { reconcileEventsDaemonAtStartup } = yield* io.promise(
        "load events daemon startup reconciliation",
        () => import("./core/events/startup.ts"),
      );
      const daemon = yield* reconcileEventsDaemonAtStartup();
      if (daemon.status === "restarted") {
        console.log("  restarted the events daemon on the current build");
      } else if (daemon.status === "failed") {
        console.error(`wt: events daemon restart failed (${daemon.detail}); starting anyway`);
      }
    }
  // Boot sentinel: record that this version is starting; core/update
  // promotes it to "known good" once it survives the health window (or
  // exits cleanly). A leftover sentinel on the next launch is evidence
  // the previous start died without tripping the catch below (native
  // crash, kill) — offer a rollback before trying again. Runs after
  // the update prompt so a just-landed fix wins over rolling back.
    if (process.env.WT_UPDATE !== "off") {
      const { maybeOfferStaleBootRollback } = yield* io.promise("load stale boot rollback", () => import("./cli/commands/rollback.ts"));
      yield* maybeOfferStaleBootRollback().pipe(
        Effect.mapError(io.wrap("run stale boot rollback")),
      );
      const { armBootSentinel } = yield* io.promise("load boot sentinel", () => import("./core/update.ts"));
      yield* armBootSentinel();
    }
    const { setWezTermTabTitle } = yield* io.promise("load WezTerm integration", () => import("./core/wezterm.ts"));
    yield* setWezTermTabTitle("wt", config.paths.weztermCli).pipe(
      Effect.catch(() => Effect.void),
    );
    if (process.env.WT_WORKSPACE === "on" && !process.env.WT_WORKSPACE_SOCKET && process.stdout.columns >= 100) {
      const { launchWorkspace } = yield* io.promise("load workspace prototype", () => import("./core/workspace.ts"));
      const workspaceCode = yield* launchWorkspace();
      if (process.env.WT_UPDATE !== "off") {
        const { completeBootSentinel } = yield* io.promise("load boot completion", () => import("./core/update.ts"));
        yield* Effect.sync(completeBootSentinel);
      }
      return workspaceCode;
    }
    const { runTui } = yield* io.promise("load TUI", () => import("./tui/runtime.tsx"));
    yield* runTui.pipe(
      Effect.mapError(io.wrap("run TUI")),
    );
    if (process.env.WT_UPDATE !== "off") {
      const { completeBootSentinel } = yield* io.promise("load boot completion", () => import("./core/update.ts"));
      yield* Effect.sync(completeBootSentinel);
    }
    return 0;
});

const program = main().pipe(
  Effect.scoped,
  Effect.catchCause((cause) =>
    Effect.gen(function* () {
      yield* Effect.sync(() => {
        console.error(renderFailure(cause));
      });
      // If this version is a fresh update that never booted healthy, offer
      // to roll back to the one that did. The offer path is config-free
      // (core/update.ts) so it works even when the crash IS the config
      // loader rejecting the user's config; it re-execs on acceptance and
      // must never mask the original error otherwise.
      const rollback = yield* io.promise("load crash rollback", () => import("./cli/commands/rollback.ts")).pipe(Effect.option);
      if (rollback._tag === "Some") {
        yield* rollback.value.maybeOfferCrashRollback();
      }
      return 1;
    }),
  ),
  // Drain the log write chain before the hard exit below. Every log
  // write is an async `appendFile`, so a short CLI command that returns
  // immediately (`wt status`, `wt section`) would otherwise exit with
  // its lines still queued — silently losing the file-only audit trail
  // those commands deliberately write, and any warning raised during a
  // read. The TUI flushes in its own shutdown path; this covers
  // everything else. Best-effort: a logging failure must never change a
  // command's exit code.
  Effect.ensuring(
    io.promise("load logger", () => import("./core/logger.ts")).pipe(
      Effect.flatMap(({ flushLogger }) => flushLogger),
      Effect.catch(() => Effect.void),
    ),
  ),
);

const code = await Effect.runPromise(program);
  // Explicit exit: the TUI path can leave behind background listeners
  // (persister sub, refetch intervals, sqlite handle) that keep the
  // event loop alive even after cleanup. A hard exit is the standard
  // CLI pattern here.
process.exit(code);
