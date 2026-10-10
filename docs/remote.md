# Remote workers

wt can use one configured SSH host as a worker. The controller owns the TUI,
remote row layout, and last-known inventory. The worker owns its worktrees,
tmux server, database, and coding-agent sessions. Filesystem and agent operations
run on the worker through its installed `wt` executable.

## Install and configure the worker

Install the native wt release on the worker using the normal release installer.
The worker does not need this source repository, Bun, or a copy of the
controller's checkout. Its configured `wt_path` must resolve to a stable wt
launcher so later native updates and rollback continue to select the active
release. The default is `~/.wt/bin/wt`.

The worker needs its own `~/.config/wt/config.toml`, pointing at its own main
clone and worktree root, with:

```toml
[instance]
role = "worker"
```

Configure the controller with the SSH destination and optional stable worker
launcher path:

```toml
[remote]
host = "builder"
label = "Builder"
wt_path = "~/.wt/bin/wt"
```

The controller uses non-interactive SSH for commands and worker snapshots. SSH
must be able to connect without an interactive password prompt. Interactive
remote TUI and session entry use a PTY and can use the SSH client's normal
interactive authentication.

## Compatibility and ownership

The hidden `_hello` handshake reports the worker role, protocol version, and
build identifier. Remote commands require worker role and an exact protocol
version match. Native wt uses protocol 3; older protocol-2 workers must be
updated before the native controller can use them. A different build with a
matching protocol remains usable and can be shown as a version warning. The
hidden `_snapshot` command returns the versioned worktree snapshot contract;
it is separate from the human-facing `wt ls --json` output. The worker reads
development-server state in one batch without running health checks. A per-row
read failure is returned as `devError` with an unknown (`null`) status.

Remote inventory identity is the SSH host plus worktree slug. Sections,
ordering, and archive state remain in the controller's local database. A
failed SSH request leaves the last successful inventory visible as unavailable;
connection failure never means that remote worktrees were deleted.

The native worker handshake, snapshot, and command dispatch are implemented.
The Rust controller's remote board projection and last-good cache are still an
integration step; until those are wired, this plumbing is available through the
remote commands rather than the remote TUI rows. Remote agent message delivery
and session controls also fail closed where the controller adapter has not yet
been connected.

Interactive sessions use a controller terminal handoff, then run the worker's
hidden `_session` command over SSH with a PTY. The worker creates or resumes the
session in its own tmux server. Agent messages re-enter the worker's ordinary
`wt agent send` routing, keeping harness sockets, delivery locks, and tmux
operations on that host.

## Updating

The worker's installed launcher selects its own native release and update
channel. Update and rollback the worker through that installation. A source
checkout on the controller is not uploaded or installed on the worker, and
remote operations do not replace the configured worker launcher.
