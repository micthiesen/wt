# GitHub webhooks

The optional events daemon uses a repository webhook to refresh the same batched GitHub query used by wt. It does not build pull request state from webhook payloads. A signed delivery is only a signal to fetch current state through the user's existing `gh` authentication.

Without `[github.events]`, wt uses its normal GitHub refresh backstop and no daemon is installed or started.

## Setup

Add an events section to the active wt configuration:

```toml
[github.events]
host = "127.0.0.1"
port = 8765
secret_file = "~/.config/wt/github-webhook-secret"
```

Then run:

```sh
wt events secret   # create a private secret file, if it does not exist
wt events install  # write this repository's per-user launchd agent
wt events start
wt events status
```

`wt events secret` does not replace an existing secret. If `secret_file` is not configured, it prints a new secret for you to add as `secret` under `[github.events]`. `install` requires a persistent secret before it writes the agent. Secret files are created with owner-only permissions.

Configure a GitHub repository webhook to send `application/json` to `https://<your-domain>/webhook`, forwarding to the configured host and port. Select `pull_request`, `pull_request_review`, `pull_request_review_thread`, `issue_comment`, `check_suite`, `check_run`, `status`, `merge_group`, and `push`. A reverse proxy or tunnel can expose the HTTPS endpoint. If another machine must reach the listener, bind to a trusted LAN address; the HMAC secret is the request authentication boundary.

The daemon accepts only the configured event types, limits the request body to 5 MiB, bounds concurrent requests and queued deliveries, and rejects requests whose `X-Hub-Signature-256` does not match the raw request body. HMAC verification uses a constant-time tag check. When its bounded queue fills, it coalesces overflow into one conservative refresh signal instead of retaining more request bodies.

## Files and freshness

The daemon writes three files under `<cache_root>/events/`:

- `github.json` contains the camelCase GitHub snapshot, covered branches, Unix-millisecond `updatedAt`, and the writer's `writerSha`.
- `github.touch` changes after a successful snapshot so an active TUI can notice fresh data.
- `state.json` contains daemon PID, port, build, start/event/fetch timestamps, accepted event count, and the most recent refresh error.

These files are separate from the SQLite query cache. A successful snapshot replaces the previous snapshot atomically. A failed branch inventory or GitHub query leaves the last good snapshot in place and records the error. Cache reads are size-bounded; malformed snapshots are ignored, and malformed daemon state is reported as unreadable rather than treated as proof that no daemon exists.

Snapshots include local worktree branches and configured remote worker branches. An unavailable remote inventory keeps its last known branch set and makes event relevance unknown, so a branch-scoped event is not discarded on incomplete information. Pull request, review, status, merge queue, and unscoped events refresh regardless of branch match. Plain issue comments are ignored; pull request comments are not.

The scheduler coalesces bursts for 1.5 seconds and enforces a 10-second minimum between fetch starts. The floor is measured from the prior fetch start, so a slow query consumes part of the interval. Continuous webhook traffic cannot postpone a pending fetch indefinitely. If the bounded request queue fills, one coalesced refresh preserves correctness.

The TUI uses a cached snapshot only when it has the current build stamp, is no more than 90 seconds old, and covers every requested branch. Otherwise it performs its normal GitHub fetch. The periodic refresh remains the recovery path for missed webhook deliveries.

## Native launchd service

The launch agent is `~/Library/LaunchAgents/com.wt.events.plist`. It invokes the stable native launcher at `~/.local/share/wt/bin/wt` (or the configured `WT_INSTALL_ROOT`), not a source checkout, Bun, or a version-specific release directory. The plist freezes absolute global and repository config selectors, the current `PATH` used to find `git` and `gh`, and logs under `<cache_root>/events/`.

There is one `com.wt.events` agent per user. `start`, `stop`, `restart`, and `uninstall` require the plist's config selectors and log paths to prove that it belongs to the active repository. Unknown ownership fails closed. `install` also refuses to overwrite a foreign or unidentifiable plist. These mutations share a per-user lock.

When wt starts with events enabled, it checks an existing owned agent in the background. A live process on the current build is left alone. A stopped process or an older build is unloaded, rewritten to use the current stable launcher and absolute config selectors, and reloaded. A foreign agent is left untouched. `wt events restart` performs the same explicit reconciliation and waits for a new live process on the current build.

Launchd management is available on macOS. `wt events serve` can also run in the foreground on other platforms for diagnostics or deployment under another service manager. It exits on SIGTERM/SIGINT and does not install a service there.

The daemon refreshes origin before each GitHub query, but a failed `git fetch origin --prune` is logged and does not prevent the GitHub refresh. Updating safe local base/`keep_fresh` refs remains owned by the shared repository freshness service and is not part of this daemon's refresh operation.
