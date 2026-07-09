import { config } from "../config.ts";
import { createLogger } from "../logger.ts";
import { run } from "../proc.ts";

const log = createLogger("[gh]");

// `which gh` is memoized so per-slice loops (stack status/rebase) don't
// re-spawn it each call — but only the POSITIVE result. A cached negative
// would pin "no gh" for the whole session even after the user installs it;
// re-probing in gh-absent mode is cheap (everything gh-backed is off anyway).
let _hasGh: boolean | undefined;
export async function hasGh(): Promise<boolean> {
  if (_hasGh) return true;
  const r = await run(["which", "gh"]);
  const found = r.exitCode === 0 && r.stdout.trim().length > 0;
  if (found) _hasGh = true;
  return found;
}

// Cache the resolved `owner/name` — it never changes for a given clone.
// Same positive-only rule as `hasGh`: a transient failure (gh not yet
// authed at startup) shouldn't pin null for the whole session.
let _repoSlug: string | null | undefined;
export async function repoSlug(): Promise<string | null> {
  if (_repoSlug != null) return _repoSlug;
  const r = await run(
    ["gh", "repo", "view", "--json", "nameWithOwner"],
    { cwd: config.paths.mainClone, timeoutMs: 5_000 },
  );
  if (r.exitCode !== 0) return null;
  try {
    const data = JSON.parse(r.stdout) as { nameWithOwner?: string };
    _repoSlug = data.nameWithOwner ?? null;
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { stdout: r.stdout.slice(0, 200) });
    return null;
  }
  return _repoSlug;
}

/**
 * The currently-authenticated GitHub user's login. Cached for the
 * life of the process — gh auth doesn't change while the TUI is
 * running. Used to filter the user out of reviewer pickers (you
 * can't review your own PR).
 */
let _authedLogin: string | null | undefined;
export async function fetchAuthenticatedLogin(): Promise<string | null> {
  // Positive-only memo (see `hasGh`): a failed probe (not yet authed)
  // re-tries on the next call instead of pinning null all session.
  if (_authedLogin != null) return _authedLogin;
  if (!(await hasGh())) return null;
  const r = await run(["gh", "api", "user", "--jq", ".login"], {
    cwd: config.paths.mainClone,
    timeoutMs: 5_000,
  });
  if (r.exitCode !== 0) {
    log.error("auth user fetch failed", {
      stderr: r.stderr.slice(0, 200),
    });
    return null;
  }
  const login = r.stdout.trim();
  if (login.length > 0) _authedLogin = login;
  return _authedLogin ?? null;
}
