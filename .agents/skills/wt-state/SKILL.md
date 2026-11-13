---
name: wt-state
description: Read-only inspection of a local wt instance's runtime state: the daily JSON app log, action run logs, the source snapshot cache, the SQLite state database, locks, the events daemon and wt's private tmux server. Use when debugging stuck rows, stale data, "why isn't this refreshing", an automation or action that misbehaved, or to see what wt did without attaching to a session.
---

# wt-state

Everything here is read-only. Never edit, delete or "refresh" these files to
fix a problem: SQLite state is authoritative, and the rest is rebuilt by wt.

## Where state lives

Paths are per repository. `<repo-id>` is the main clone's path relative to
home, lowercased with every non-alphanumeric run replaced by `-`
(`~/Code/My.App` gives `code-my-app`). `[paths]` in the config can override
each one (`docs/configuration.md`); a setup with no repository `.wt.toml` uses
`~/.cache/wt/` itself as the cache root.

    ROOT=~/.cache/wt/<repo-id>      # cache root; `ls -dt ~/.cache/wt/*/logs/app` finds live ones

| Path | Contents |
| --- | --- |
| `$ROOT/logs/app/wt-native.YYYY-MM-DD.log` | App log: JSON lines, UTC, rotated daily at UTC midnight, 7 files kept |
| `$ROOT/logs/actions/<run-id>/` | One action run: `meta.json`, `stream.log`, `stderr.log`, `done.json` |
| `$ROOT/logs/<slug>-<time>.log` | Worktree removal logs |
| `$ROOT/query/*.json`, `$ROOT/remote-query/` | Last snapshot of each board source (`git`, `github`, ...); display-only cache |
| `$ROOT/events/` | Events daemon: `state.json`, `github.json`, `daemon.out.log`, `daemon.err.log` |
| `$ROOT/locks/*.lock` | Held operation locks, per slug or `__name__` |
| `$ROOT/automations.json` | Automation ledger (handled fires, breakers, deferrals) |
| `$ROOT/dev/`, `$ROOT/manager/`, `$ROOT/harness.json`, `$ROOT/*-sessions.json` | Dev servers, manager reports, agent session records |
| `~/.local/state/wt/wt.sqlite` | Authoritative state for all repositories, partitioned by `repo_id` |

Prefer the CLI where it answers the question: `wt ls`, `wt status`,
`wt automations`, `wt events status`, `wt dev status`, `wt doctor [<slug>]`.

## App log

Each line is `{"timestamp", "level", "fields": {"message", ...}, "target"}`.
Timestamps are UTC, so "today" may be in yesterday's or tomorrow's file.
Lines carrying `fields.event_text` are what the activity pane showed;
`fields.event_channel == "attention"` marks the attention feed. The default
level is `info`; `WT_LOG=debug` (a tracing filter) at launch records more.

    LOG=$ROOT/logs/app/wt-native.$(date -u +%F).log
    jq -c 'select(.level == "WARN" or .level == "ERROR")' "$LOG" | tail -50
    jq -r 'select(.fields.event_text) | "\(.timestamp) \(.fields.event_text)"' "$LOG" | tail -50
    jq -r '.target' "$LOG" | sort | uniq -c | sort -rn | head    # what is noisy
    grep -h '<slug>' "$ROOT"/logs/app/wt-native.*.log | jq -c '[.timestamp, .level, .fields]'

## State database

    sqlite3 -readonly ~/.local/state/wt/wt.sqlite '.tables'
    sqlite3 -readonly ~/.local/state/wt/wt.sqlite 'select * from repositories'

Open it `-readonly`; a running wt holds it in WAL mode.

## Tmux

Agent, shell, diff, dev and action sessions run on a private server whose
socket is `wt-<repo-id>` (`wt` without a repository `.wt.toml`; `[tmux].socket`
or `WT_TMUX_SOCKET` overrides it):

    tmux -L wt-<repo-id> list-sessions -F '#{session_name}	#{pane_current_command}	#{pane_current_path}'

A session is named for its slug, with `-diff`, `-shell` or similar suffixes
for the other kinds. Use `capture-pane -p -t <session>` to read one; do not
`attach` or `send-keys`.
