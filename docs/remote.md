# Remote workers

wt combines local worktrees and multiple SSH hosts in one board. Each host runs
the same Rust sources and command handlers. A persistent SSH connection carries
prepared snapshots and typed commands; there is no separate remote feature
implementation. The controller owns terminal input, desktop applications, and
board layout. Each host owns its worktrees, database, tmux server, and agents.
An unavailable host keeps its last known rows with a visible error while the
other hosts continue updating.

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
protocol 1. The
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
