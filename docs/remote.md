# Remote workers

wt combines local worktrees and multiple SSH hosts in one board. For a host
you control, start with [managed remotes](#managed-remotes): one config line,
and the controller provisions the rest. Each host runs
the same native application and command handlers. A persistent SSH connection
carries prepared snapshots and typed commands; there is no separate remote
feature implementation. The controller owns terminal input, desktop
applications, and board layout. Each host owns its worktrees, database, tmux
server, and agents.
An unavailable host keeps its last known rows with a visible error while the
other hosts continue updating. Its last snapshot is also restored at launch,
marked `cached` until the host answers.

## Managed remotes

A managed remote is the easiest way to use another machine. You turn it on
with one line, and the machine running the TUI provisions everything the
worker needs:

```toml
[[remotes]]
host = "beast"        # SSH destination or ~/.ssh/config alias
label = "Beast"
managed = true
```

There is no worker config to write, no clone to make, and no `wt` to install
on the worker. Managed remotes are opt-in: entries without `managed = true`
keep working exactly as described in the rest of this page.

The design goal is that the worker needs only an SSH account, `git`, and the
coding agent (`codex`). Everything else comes from the controller, and anything
that genuinely needs a person (a browser sign-in) is asked for when it is
first needed, in the TUI. If you never use a capability, wt never asks.

### What the controller provisions

Connecting converges the worker on the controller. All
managed state lives under one directory per repository,
`~/.local/share/wt/managed/<repo-id>/`, so the worker account's own
`~/.codex`, `~/.config/gh`, `~/.gitconfig` and wt config are never read or
changed. On a machine you also use directly, your personal Codex and GitHub
accounts keep working there unchanged.

| What | How |
|---|---|
| wt | The build-matched runtime, linked at `bin/wt` so agents can call `wt` by name. |
| Worker config | Generated from the controller's merged config with worker paths; remotes, the update channel, the webhook daemon, and the tmux socket are left out. |
| Repository | `repo/` is cloned over HTTPS from the controller's `origin`; SSH GitHub URLs are rewritten to HTTPS. A partial clone never looks complete. |
| Project files | Untracked files listed in `lifecycle.env_files_to_copy` and `copy_globs` (for example `.env`) are copied from the controller's main clone. Files tracked on either side, and paths through a symlink, are never written. |
| Git | `gitconfig` copies the controller's global settings except credentials, signing, includes, URL rewrites, HTTP and email settings, and paths to controller programs (editor, hooks, askpass, templates). A credential helper answers `github.com` HTTPS requests from the managed token. |
| GitHub | `gh auth token` from the controller's main clone, written for both git and `gh` (`gh/hosts.yml`). |
| Codex | `codex/` holds the controller's `config.toml` (credentials in a file, managed paths trusted), `AGENTS.md`, `hooks.json`, `rules/`, and skills from `$CODEX_HOME/skills` and `~/.agents/skills`. |
| Other secrets | `[managed.env]` commands run on the controller; each output becomes a variable on the worker. |
| PATH | The worker's login-shell PATH is captured, so Homebrew and version managers work in sessions started over SSH. |

`env.sh` sets `CODEX_HOME`, `GH_CONFIG_DIR`, `GIT_CONFIG_GLOBAL`, `PATH`, and
the `[managed.env]` variables before every worker command. Agent tmux sessions
inherit the paths and source `secrets/env.sh` when they start, so secret
values never appear in process arguments. Secrets are written with mode
`0600` inside a `0700` directory. Skills and rules that Codex creates on the
worker itself are kept when the controller's copies change. Signing out of
`gh` on the controller removes the token from the worker on its next
connection.

A worker that reconnects within ten minutes of its last provisioning reuses
it; later reconnects provision again, so a rotated token or an edited setting
reaches the worker without a restart.

```toml
[managed.env]
CLOUDFLARE_API_TOKEN = "op read op://Dev/Cloudflare/token"
NPM_TOKEN = "security find-generic-password -s npm -w"
```

Prefer API tokens over OAuth logins for these. A copied OAuth refresh token
breaks once either machine refreshes it, which is also why Codex is not
copied.

### Sign-ins

Codex keeps its own sign-in on the worker. Copying the controller's
`auth.json` would let the two machines invalidate each other's rotating
refresh token, so the worker signs in once and refreshes on its own. Agents
keep running while the controller sleeps.

wt asks for a sign-in only when a request needs it:

- Starting or entering a Codex agent on a host whose Codex is signed out
  opens `Sign in on <host>`. Confirming runs `codex login` on the worker
  over SSH and forwards port 1455, so the link it prints completes in the
  controller's browser. wt then checks again and continues the original
  request.
- Creating a worktree on a host without GitHub credentials offers `gh auth
  login` on the worker. Signing in to `gh` on the controller instead lets wt
  supply the token automatically, which is the usual case.

Owed sign-ins and degraded capabilities (for example `gh` not installed) also
appear in the attention feed and as a note in the `Ctrl+N` host picker.

### Choosing where a worktree runs

`n` opens `New worktree on <host> (Tab: host):`. `Tab` cycles through this
machine and every remote, and `n` starts on the host chosen last. A stacked
create (`N`) stays on its parent's host. Rows from every host share one board;
the host label on each row is the only difference.

### Current limits

- Managed remotes are tested with a macOS controller and macOS or Linux
  workers. A worker whose target differs from the controller's needs the
  controller to run an installed release, as for any remote; a development
  build can only provision a worker of its own target.
- The repository must be on GitHub, and the worker must be able to reach it.
- Commits made on the worker are unsigned, because the controller's signing
  configuration and key stay on the controller.
- The worker must already have `codex` (and preferably `gh`) on its
  login-shell PATH, for example from Homebrew. wt reports a missing tool.
- Dev servers run on the worker; open their URLs from the worker, or forward
  the port with SSH.

## Configure the worker

The worker does not need this source repository, Bun, or a preinstalled wt
binary. It needs an SSH account with a POSIX shell, `uname`, a SHA-256 utility
(`sha256sum` or `shasum`), and permission to write under its home directory.
The controller probes the worker's OS and architecture, then stages a native
executable under `~/.cache/wt/native-runtimes/<target>/<sha256>/wt`. It checks
the content hash, boot identity, worker role, protocol, and build before
publishing the executable atomically. Runtime paths are content-addressed and
per-client; provisioning never changes the worker's configured `wt_path`.

For a worker with the same target as the running controller, wt verifies and
uses the controller executable. For a different target, wt downloads the exact
matching artifact from the controller build's published release and verifies
it before transfer. A development or unpublished build cannot provision a
cross-target worker; wt reports that matching published native artifacts are
required.

The worker needs its own `~/.config/wt/config.toml`, pointing at its own main
clone and worktree root, with:

```toml
[instance]
role = "worker"
```

Configure the controller with one or more SSH destinations:

```toml
[[remotes]]
host = "builder"
label = "Builder"

[[remotes]]
host = "linux-worker"
label = "Linux"
config = "~/.config/wt/project.toml"
```

The legacy single `[remote]` table remains supported and is normalized into the
same list. An optional `wt_path` remains unchanged and is available to callers that
explicitly use the stable worker launcher. Native remote operations bind to the
verified runtime for the life of that client. Non-interactive SSH must connect
without an interactive password prompt. Interactive remote TUI and session
entry use a PTY and can use the SSH client's normal interactive authentication.

Each wt process loads exactly one configuration. Start separate controller
processes with `WT_CONFIG=/absolute/config.toml` for separate projects. A remote
entry's optional `config` selects one worker config, independently of the
worker's default and inherited shell environment. It accepts an absolute path
or `~/...`; expansion happens on that worker. Changing it also changes the
remote row identity, so layout or confirmations for one config cannot target
another. The runtime binary can be shared because it is immutable; project
state, locks, and sessions follow the selected config.

With multiple hosts, `wt remote --host builder <command>` selects a destination.
Omitting `--host` is accepted only when exactly one host is configured. The
TUI asks which host should own a new worktree or a cleanup operation, and
row commands stay bound to the selected row's host throughout confirmation.

## Compatibility and ownership

The hidden `_hello` handshake reports the worker role, protocol version, and
build identifier. Remote commands require worker role and an exact protocol
version match. The controller provisions its exact native build automatically;
the streamed host protocol also verifies that exact build before admitting
commands. Independently installed or older worker launchers do not have to be
manually updated first. Native wt uses worker protocol 3 and host-stream
protocol 3; protocol 2 added each host's per-source provenance and observation
stamps. Protocol 3 adds cleanup identities selected from the background board;
the worker checks confirmed evidence and current hazards before removal. The
hidden `_snapshot` command returns the versioned worktree snapshot contract;
it is separate from the human-facing `wt ls --json` output. The worker reads
development-server state in one batch without running health checks. A per-row
read failure is returned as `devError` with an unknown (`null`) status.

Remote inventory identity is the SSH host, selected config, and worktree slug. Sections,
ordering, and archive state remain in the controller's local database. A
failed SSH request leaves the last successful inventory visible as unavailable;
connection failure never means that remote worktrees were deleted.

The controller validates each complete worker snapshot; malformed or duplicate
rows reject the whole response. Reads reconnect automatically. Commands are
bound to their connection and are never replayed after a lost reply: if a write
may have happened, wt reports the ambiguity. A disconnected host cannot turn a
pending confirmation into an operation on a different host or configuration.

Interactive sessions use a controller terminal handoff, then run the worker's
hidden `_session` command over SSH with a PTY. The worker creates or resumes the
session in its own tmux server. Agent messages re-enter the worker's ordinary
`wt agent send` routing, keeping harness sockets, delivery locks, and tmux
operations on that host.

## Updating

The controller selects the version for its remote connections. On upgrade or
rollback, its next connection provisions that exact build and binds to its
immutable path. Existing controllers remain bound to their existing builds.
Provisioning does not restart tmux, terminate agents or action workers, or
replace a running executable in place. Old runtimes are retained so ongoing
work can finish; there is currently no automatic runtime-cache eviction.

A worker's separate installed launcher may still serve a local interactive wt
session on its own channel. That does not determine the version used by a
controller connection. No wt source checkout or JavaScript runtime is uploaded.
