import { copyFileSync, existsSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import { createLogger } from "../../logger.ts";

const log = createLogger("[claude:trust]");
const CLAUDE_JSON = join(homedir(), ".claude.json");

/**
 * Mark `wtPath` as trusted in Claude Code's `~/.claude.json` so opening a
 * session there doesn't hit the "Do you trust this folder?" prompt (which
 * also drops the worktree's `.claude/settings.json` allow rules until
 * accepted).
 *
 * Only rift worktrees need this: they're independent clones, so Claude
 * sees each as a brand-new project, whereas a git worktree resolves to
 * the already-trusted main repo. Mirrors the fleet approach in
 * `unseamless-coop/scripts/fleet/worker-new` — set
 * `.projects["<path>"].hasTrustDialogAccepted = true`.
 *
 * Best-effort and idempotent: a no-op when already trusted, so the common
 * case does NOT write — bounding any clobber race against Claude's own
 * live writes to the first spawn per workspace. Every error is swallowed;
 * trust bookkeeping must never block a session spawn (worst case, Claude
 * shows its prompt once, exactly as before).
 */
export function trustClaudeWorkspace(wtPath: string): void {
  try {
    // Claude seeds this file itself on first run; if it's absent, there's
    // nothing to edit and Claude will create + prompt on its own.
    if (!existsSync(CLAUDE_JSON)) return;
    const data = JSON.parse(readFileSync(CLAUDE_JSON, "utf8")) as Record<string, unknown>;
    if (typeof data !== "object" || data === null) return;

    const projects = (data.projects ??= {}) as Record<string, Record<string, unknown>>;
    const proj = (projects[wtPath] ??= {});
    if (proj.hasTrustDialogAccepted === true) return; // already trusted — no write
    proj.hasTrustDialogAccepted = true;

    // Backup + atomic temp-then-rename, same discipline as the fleet, so a
    // clobber of Claude's concurrently-written config stays recoverable.
    try {
      copyFileSync(CLAUDE_JSON, `${CLAUDE_JSON}.bak`);
    } catch {
      // A missing/locked backup target must not abort the trust write.
    }
    const tmp = `${CLAUDE_JSON}.wt-${process.pid}.tmp`;
    writeFileSync(tmp, `${JSON.stringify(data, null, 2)}\n`);
    renameSync(tmp, CLAUDE_JSON);
    log.debug("trusted rift workspace in ~/.claude.json", { wtPath });
  } catch (err) {
    log.warn("could not set claude workspace trust", {
      wtPath,
      err: err instanceof Error ? err.message : String(err),
    });
  }
}
