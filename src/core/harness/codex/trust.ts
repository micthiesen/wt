import { appendFileSync, existsSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import { createLogger } from "../../logger.ts";

const log = createLogger("[codex:trust]");

function codexConfigPath(): string {
  return join(process.env.CODEX_HOME ?? join(homedir(), ".codex"), "config.toml");
}

function projectHeader(wtPath: string): string {
  return `[projects."${wtPath}"]`;
}

/**
 * Trust `wtPath` for Codex by persisting a `[projects."<path>"]` table with
 * `trust_level = "trusted"` in config.toml — the analogue of Claude's
 * `~/.claude.json` trust for rift checkouts (independent clones Codex reads
 * as new projects). Codex's onboarding gate reads ONLY this persisted entry
 * (a session `-c` override is ignored), so the file must be written.
 *
 * Idempotent (no-op when the header already exists) and best-effort (a
 * missing config just means Codex prompts once, exactly as before). Writes
 * THROUGH the file rather than temp+rename: config.toml is commonly a stowed
 * dotfiles symlink, and a rename would replace the link with a plain file —
 * an in-place append preserves it. Mirrors `unseamless-coop/scripts/fleet/_codex`.
 */
export function trustCodexWorkspace(wtPath: string): void {
  try {
    const cfg = codexConfigPath();
    if (!existsSync(cfg)) return; // codex seeds this itself; it'll prompt once
    const header = projectHeader(wtPath);
    if (readFileSync(cfg, "utf8").split("\n").includes(header)) return; // already trusted
    appendFileSync(cfg, `\n${header}\ntrust_level = "trusted"\n`);
    log.debug("trusted rift workspace in codex config", { wtPath });
  } catch (err) {
    log.warn("could not set codex workspace trust", {
      wtPath,
      err: err instanceof Error ? err.message : String(err),
    });
  }
}

/**
 * Remove `wtPath`'s `[projects."<path>"]` table (header plus its keys, up to
 * the next table header or EOF) from config.toml, so the stowed config doesn't
 * accumulate dead-workspace entries after a rift checkout is torn down. No-op
 * when absent. Overwrites in place (preserves the symlink); best-effort.
 */
export function untrustCodexWorkspace(wtPath: string): void {
  try {
    const cfg = codexConfigPath();
    if (!existsSync(cfg)) return;
    const header = projectHeader(wtPath);
    const lines = readFileSync(cfg, "utf8").split("\n");
    if (!lines.includes(header)) return; // nothing to remove
    const out: string[] = [];
    let skip = false;
    for (const line of lines) {
      if (line === header) {
        skip = true; // drop the header line
        continue;
      }
      if (skip && line.startsWith("[")) skip = false; // next table — stop, keep it
      if (skip) continue; // a key inside the dropped table
      out.push(line);
    }
    writeFileSync(cfg, out.join("\n"));
    log.debug("removed codex trust entry", { wtPath });
  } catch (err) {
    log.warn("could not remove codex workspace trust", {
      wtPath,
      err: err instanceof Error ? err.message : String(err),
    });
  }
}
