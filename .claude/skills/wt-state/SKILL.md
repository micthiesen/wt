---
name: wt-state
description: Read-only inspection of the local wt instance — TanStack Query cache (~/.cache/wt/cache.sqlite), wt-private tmux sessions, JSON state files, locks, and the daily app log. Use when debugging stuck rows, stale data, "why isn't this refreshing", or peeking at runtime state without attaching to a session.
---

# wt-state

Read-only scripts for inspecting the running wt instance's local state.
Hardcoded for this user's setup; assumes `~/.cache/wt/` and
`~/.config/wt/config.toml` exist.

All scripts live in `scripts/` next to this file. Bash, no install step.
Requires `sqlite3`, `jq`, `tmux`, `bun` (all already on the user's box).

## Snapshot

`scripts/snapshot` — one-shot overview: `wt ls`, live tmux sessions
(per kind), held locks, archived slugs, last 20 lines of today's app log.
Start here.

## Cache (cache.sqlite)

`scripts/cache <subcommand>`:

- `keys [pattern]` — list IDs with size + age, optional substring filter
- `get <pattern>` — pretty-print one matching entry (errors on multi-match)
- `summary` — counts + bytes grouped by key shape

Key shapes seen in the wild: `worktrees`, `tmuxSessions`, `archive`,
`wtState`, `fetchOrigin`, `wt/*/dirty|deploy|diffContext|gitActivity`,
`github/*` (keyed by branch list), `stack/*`, `aiSummary/*` (content hash).

## Tmux

`scripts/tmux` — sessions on the `-L wt` private server, classified as
claude (`<slug>`), diff (`<slug>-diff`), or shell (`<slug>-shell`), with
each pane's cwd and current command. "(no server running)" is the steady
state until the first F12.

## Logs

`scripts/logs [options] [pattern]` — searches
`~/.cache/wt/logs/app/wt-YYYY-MM-DD.log`. Defaults to today, last 50 lines.

- `-d, --date YYYY-MM-DD` — different day
- `-e, --errors` — ERROR lines only
- `-E, --events` — EVENT lines only (what the activity pane showed)
- `-n, --lines N` — different tail size (default 50)
- `-f, --follow` — `tail -f`
- positional pattern — substring grep, applied last

## Conventions

- Cache, locks, and log paths are not user-overridable in the loader, so
  the scripts hardcode `~/.cache/wt/`.
- For worktree listings, `snapshot` shells out to `bun src/main.ts ls`
  in `$WT_REPO` (default `~/.wt`). Override `WT_REPO` if it moves.
- Strictly read-only. No write paths exist.
