#!/usr/bin/env bun

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

async function main(): Promise<number> {
  const argv = process.argv.slice(2);

  // Args given → dispatch to CLI.
  if (argv.length > 0) {
    const { dispatch } = await import("./cli/index.ts");
    return dispatch(argv);
  }

  // No args + non-TTY → fall back to `ls` (matches the old Python tool's
  // behavior for piped/scripted use).
  if (!process.stdout.isTTY) {
    const { dispatch } = await import("./cli/index.ts");
    return dispatch(["ls"]);
  }

  // No args + TTY → interactive TUI. Every user action runs in-TUI now
  // (no CLI handoff for `new` or `clean`), so this is a single call.
  const { config } = await import("./core/config.ts");
  const { setWezTermTabTitle } = await import("./core/wezterm.ts");
  await setWezTermTabTitle("wt", config.paths.weztermCli);
  const { runTui } = await import("./tui/runtime.tsx");
  await runTui();
  return 0;
}

try {
  const code = await main();
  // Explicit exit: the TUI path can leave behind background listeners
  // (persister sub, refetch intervals, sqlite handle) that keep the
  // event loop alive even after cleanup. A hard exit is the standard
  // CLI pattern here.
  process.exit(code);
} catch (err) {
  console.error(err instanceof Error ? err.stack ?? err.message : String(err));
  process.exit(1);
}
