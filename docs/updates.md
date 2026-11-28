# Updates, rollback, and stored data

Installed wt releases are native binaries fetched from GitHub Releases. Updates
do not pull or modify a source checkout. The installer and updater verify the
release manifest, archive checksum, build identity, and target before activation.
The stable launcher lives under `~/.local/share/wt` by default; set
`WT_INSTALL_ROOT` to select another install root. The release manifest and
supported targets are documented in [the distribution design](rust-distribution-design.md).

Stable follows the latest non-prerelease release. Preview follows the latest
`preview-<fullsha>` prerelease. Pushes to `main` no longer publish one; publish
a preview of `main` on demand with `gh workflow run release.yml --ref main -f
publish=true`, which runs the same checks and packaging as a tag.
`wt update --channel stable|preview` persists a channel choice; otherwise the
saved channel is used. `wt update --check` checks
metadata without installing. `wt update --release <tag>` selects one exact
release for testing, including `rust-test-*` tags. Those test tags are never
selected by normal stable or preview discovery. `wt update log` prints local
update history. The old `--head` source-update option is rejected.

An update takes effect for new processes only. A running board keeps its
binary until it restarts. So does every agent session wt started before the
update: wt puts the running version's own `bin` directory
(`~/.local/share/wt/versions/<version>/bin`) first on each session's `PATH`, so
the session's agents and the commands they run keep using the wt build that
started them. A long-lived session, such as the manager, keeps calling that
older wt until it is restarted. To reach the new version from such a session,
call the stable launcher by its path (`~/.local/bin/wt`).

A pinned wt that is no longer the installed build says so on stderr, naming
the installed release and the launcher path, at most once an hour per tmux
pane. The board, wt's hidden worker commands and `--json` invocations stay
silent, so no machine-read output gains a line. Only builds that carry the
notice print it: a session started by v0.1.69 or older stays quiet.

`wt install` installs a verified release and can create the `~/.local/bin/wt`
launcher link. `scripts/install.sh` is the bootstrap installer for machines
without an existing wt executable. `wt rollback [<release-or-sha>]` activates a
previously installed version; it does not replay user arguments or infer
success from an ordinary command exit code.

## Installation and boot state

Each verified build is stored in an immutable version directory. Install state
is a bounded JSON file under the install root and records the selected channel,
current and last-good build identities, pending boot attempt, declined build,
daily check time, and a bounded operation history. A build identity includes
release tag, full build ID, and target architecture.

The launcher performs a config-free boot probe before dispatching user
arguments. It checks the candidate's compiled build and target. A new version
is pending until the application confirms startup using the same unique attempt
token. A process whose token is not the pending attempt, such as a worker
started by a tmux server that inherited an earlier launch's token, confirms
nothing, changes no state and runs normally. If the pending binary fails
before confirmation, the launcher selects the prior fallback on the next
launch. Once an ordinary command has started,
its exit status does not trigger rollback or argument replay. Explicit early
startup failure can restore the fallback immediately. On Unix the launcher
dispatches with `exec`, so the application keeps the launcher's PID and
receives signals sent to it (launchd's SIGTERM, terminal hangups) directly.

Update-state transitions use an OS file lock and atomic state-file replacement.
Every launcher start holds that lock for one read, so a transition waits up to
3 seconds for it rather than failing as busy; a second installer is refused by
the separate install lock.
Downloads and archive validation happen before activation. Extraction rejects
path traversal, links, unexpected files, and mismatched size or digest. The
candidate is placed in its immutable directory before state points at it, so a
crash may leave an unused version directory but cannot select incomplete files.

The native release and remote provisioning tests strip copies of debug binaries
before packaging or transferring them.
On macOS, it removes debug and local symbols so the fixture stays below the
128 MiB limit for each binary. The original build files keep their symbols.

After `wt update` or the startup offer installs a build on macOS, and only if `~/Library/LaunchAgents/com.wt.events.plist` exists, wt runs the new launcher's `wt events reconcile` (90-second timeout) so an installed GitHub events daemon moves onto the new build. If that fails, wt warns and suggests `wt events restart`; the update stays installed. See [github-events.md](github-events.md#native-launchd-service).

The startup offer checks at most once per day. `[update] startup_check = false`
or `WT_UPDATE=off` disables it. A declined build remains suppressed until a
newer build appears; explicitly running `wt update` is the reapply path.

While the TUI is open it also checks quietly, a minute after launch and then
every six hours, and shows a dim `↑ v0.1.40 available` at the right of the
title bar when a newer release exists, or `↑ restart for v0.1.40` when it is
already installed but this process still runs the older build. This check only
reads install state: it does not stamp the daily check, download anything, or
honor a declined build, and a failed check shows nothing. `WT_UPDATE=off` and
remote workers (`[instance] role = "worker"`) disable it; a source build run
outside a native install shows nothing.

## Contributor validation and release gate

The complete native end-to-end suite has one entry point shared with the Linux
and macOS CI jobs: `python3 scripts/native-check.py`. It runs installation,
release recovery, terminal liveness, optimistic creation and navigation,
overlapping removal menus and quit, session handoff, lifecycle, cleanup,
resource ownership, dev servers, actions, remote workers, host reconnect,
commands and stages, plus the macOS events-service check on macOS. Child output
streams directly to the terminal; a failing check stops the suite with its exit
status and name. The runner prints the retained diagnostics directory.
`--jobs N` (or `WT_NATIVE_JOBS`) runs N checks at once: each check owns its
temp home, sockets and tmux server. Each check's output is then printed whole
when it finishes, every failure is reported, and checks that measure CPU and
memory limits or terminal responsiveness run last, one at a time (see
[ci.md](ci.md#native-end-to-end-checks)). CI runs one check per runner core.

Install the pinned Rust toolchain, Python 3, tmux, SSH tools and lsof first
(Linux CI also installs openssh-server). From the repository root, set
`WT_BUILD_ID` to the full candidate commit SHA, then run:

```sh
cargo build --workspace --bins --examples --locked
python3 scripts/native-check.py --binary target/debug/wt --output /tmp/wt-native-validation
```

On Linux, the runner includes `scripts/native-workload-check.py`. CI starts
the systemd user manager before the suite. It checks a configured dev launch, child cgroup membership,
CPU throttling, hard memory-limit termination, signal propagation, and refusal
to start without resource tools. Its fixture uses a private repository, home,
tmux server and resource group.

The runner requires `WT_BUILD_ID` and existing `wt` and `wt-launcher` binaries.
It probes the candidate and refuses a build identity that differs from
`WT_BUILD_ID`, so an older local binary cannot silently pass as the candidate.
`--build` performs the same prerequisite Cargo build when needed; CI keeps its
separate cached build step. The existing bootstrap fixture also builds its
launcher. Unit tests and lint are separate gates: run the workspace nextest and
doc tests, formatting, Clippy and dependency checks as defined in Rust CI.
Running only those gates does not verify native end-to-end behavior.

To publish a stable release:

1. Run `cargo xtask gate` and the native suite against the candidate.
2. Bump `version` in the root `Cargo.toml`, run `cargo update -w`, and commit
   with the release notes as the message: `Release vX.Y.Z: <summary>`, then a
   blank line and bullet points. The summary and bullets become the GitHub
   release notes and the Discord #updates post (see [discord.md](discord.md)).
3. Rebuild with `WT_BUILD_ID` set to the release commit SHA and run the native
   suite again; check its exit status rather than piping it.
4. Push `main`, then push an annotated `vX.Y.Z` tag (message `wt vX.Y.Z`) on
   that commit. The tag's `Native release` run repeats every unit, lint, native
   and packaging check and publishes only if all of them pass.
5. When it publishes, check that `SHA256SUMS` verifies the downloaded assets
   and that `wt-release.json` names the release commit as `build_id`.

There is no separate verification run: publication already depends on the tag
run's complete checks, so a failure publishes nothing. If the tag run fails,
fix forward on `main`, delete the unpublished tag (`git push --delete origin
vX.Y.Z`), and tag the fixed commit with the same version. A flaky check can be
re-run on the same run. `workflow_dispatch` with `publish=false` remains for
validating a branch without publishing.

## Durable application data

Application state and release state are separate:

- **Repository state** lives in SQLite (`~/.local/state/wt/wt.sqlite` when the
  repository has its own `.wt.toml`; otherwise it is under that process's cache
  root). Rows are scoped by a repository-derived ID. It holds user-authored
  worktree status, sections, issue overrides, fork bases, archives, removed
  history, and related state. `wt-store` maintains SQL schema migrations and a
  separate forward-only versioned payload migration. Unknown payload fields
  are retained. See [configuration](configuration.md#repository-identity-is-a-property-of-the-repository-not-of-your-shell).
- **Update state** is under the install root and is shared by the launcher and
  installed executable. It records build transitions, not repository data.
- **Derived caches** under the repository cache root include event snapshots,
  remote presentation snapshots, and rebuildable runtime data. `Ctrl+R` clears
  the picker cache before requesting live source reads; it does not clear SQLite or accepted action and
  automation history.
- **User configuration** is hand-written TOML. wt reads and validates one
  merged configuration at process startup and never rewrites it. Configuration
  changes take effect in a new process.

`wt state migrate` imports attributable legacy JSON records into the selected
repository's SQLite state. It makes backups, uses a transaction, is safe to run
again, and removes only successfully imported legacy rows. `--keep-legacy`
performs a copy-only pass; `--from <dir>` selects a relocated legacy cache.
Migration is explicit and separate from release updates.

Do not roll back across a release that changed durable data and assume an older
binary will merge writes made after the rollback. Older binaries may not know
about newer fields or migrations. Keep migration backups and restart long-lived
wt processes when changing versions that alter storage contracts.

## Leftovers and the daily maintenance pass

wt writes files that outlive what they were for, and one pass removes them. A
running wt (the TUI process) runs it five minutes after it starts and then at
most once a day for each repository, whichever process gets there first
(`<cache root>/maintenance.json` records when, and a pass holds the
`__maintenance__` lock so two never run together). `wt doctor` counts what
`wt doctor --prune` would remove, and `wt doctor --prune` runs the pass now:
it takes the same lock (a pass already running makes it say so and do
nothing) and counts as the day's pass. Each pass writes one `maintenance pass
removed leftovers` line to the app log with the counts, the version
directories and slug records it removed, the checks it re-dated, and the
classes it could not judge.

[`[maintenance]`](configuration.md#maintenance) switches the daily pass off
(`enabled = false`) and decides whether it may touch the repository state
(`state_records`, off by default: neither the slug records nor the one-time
check re-dating below is written without it). `wt doctor --prune` ignores
both: a person asked.

Every rule needs proof that nothing owns the file. A worktree is alive when
`git worktree list` names its directory, wherever it is, or the inventory
holds it. When the worktree list, the tmux sessions or the process list
cannot be read, the classes that depend on it are skipped, never guessed, and
`wt doctor` counts only what a pass would in fact remove.

| What | Removed when |
|---|---|
| Installed versions (`<install root>/versions/<key>`) | not the current or last-good build, not part of a pending boot, not named by any running process (its executable or its environment) or by the environment of any session on wt's tmux server (a session stays pinned through its `PATH` to the build that started it, even while nothing runs in it), and not one of the three most recently installed of the rest. If `ps`, the session list or any session's environment cannot be read, no version is removed that pass. Done under the install state's lock; a busy lock removes nothing. `wt rollback` reaches only versions still installed |
| Action runs (`<log dir>/actions/<run id>`) | finished, started over 30 days ago, not among the newest 500 runs, and no worker can be alive: the action worker lock of the run's worktree (`action-worker-<hash>`) is free, and so is an older build's per-run lock (`action-run-<run id>`). A run whose `meta.json` names no worktree is kept |
| Dev-server files (`<cache root>/dev/<slug>.state`, `.state.attempts`, `.crash.log`) | the slug has no worktree and the file is 7 days old |
| Inspector sockets and session logs (`<cache root>/insp/<session>.sock`, `sessions/<session>.err`) | no tmux session has that name, no worktree's slug starts it, and the file is 7 days old |
| Slug records in the repository state | only by `wt doctor --prune`, or daily with `state_records = true`; see below |

A repository without its own `.wt.toml` shares its cache root, tmux socket
and lock directory with every other such repository. There the dev-server
files, inspector sockets and session logs are never removed, because they are
named by slug or session alone and another repository's would look ownerless;
`wt doctor` says so. Installed versions and finished action runs are still
pruned: neither rule asks whose they are. Each repository then has its own
stamp (`maintenance.<repo id>.json`) and lock, so every one gets its daily
pass.

Lock files are never removed. A lock is its file's inode, a locker keeps the
file after release and does not look at it again once open, so unlinking one
could let two processes hold "the same" lock. They are empty files. Action
runs share fixed lock names (one worker lock per worktree, 64 state locks),
so runs add no files; the per-run `action-run-*` and `action-state-<run id>`
files older builds created stay.

A worktree's record in the repository state outlives the worktree on purpose:
restoring its branch reads the recorded fork base, and a pause is remembered
on it. A record is dropped only when all of these hold: the slug has no
worktree in a list read immediately before the write, is not reserved
(`main`, `manager`, `wt`, `dotfiles`) and is not a remote row's; no removal
record names it (so not while the Landed block, cleanup or a restore can
still read it); its status carries no check, gate or verification, no due
step names it, and no watch is on it or was set by it; its automations are
not paused; no live worktree's fork base names its branch; it has a
`createdAt` (a record being created does not yet); and the newest time
anywhere in it is over 30 days old. The decision is made on the state as it
is inside the write transaction. A record that still carries an old check
keeps the record forever: nothing proves that check moot.

The first pass that may write the state also corrects one thing, once (the state records that it
ran). A production check that was already due when wt began recording when
checks come due (v0.1.77) was dated with the time that build first started,
so its "due for" read short. Such a check (it has no `sinceMs`) is moved
back to the last time its history shows the work in production at or after
the check was last owed. Where history cannot say, the check is left as it
is: a check closed and owed again, one owed only after the work had shipped,
one no status event names, and every staging check, which with a deploy probe
came due at a deploy that history does not record. The log line and
`wt doctor --prune` name each check moved.

Every pass checkpoints the state database. Each writable connection sets
`journal_size_limit` to 1 MiB, so the write-ahead log is cut back once a
checkpoint has emptied it instead of keeping the size of its largest burst,
and the TUI process checkpoints once more as it exits.

Never removed, and sized once by `wt doctor`: the dated `wt-YYYY-MM-DD.log`
files and `state.json.bak*` / `archive.json.bak*` backups from before the
native rewrite, and the migration archive under `<install root>/migrations`.

## Moving an existing source installation to native wt

Close the old wt boards and stop their events service before importing state;
leave tmux agents and worktrees running. Keep the old source checkout in place
until the native installation is verified. Install the chosen published release
with `scripts/install.sh --release <tag> --path`. This switches a recognized
`~/.local/bin/wt` source link only after archiving the old checkout and recording
the prior link under `<install-root>/migrations/`. Without `--path`, installation
does not inspect or change that link. Unrelated PATH entries are refused.

For each repository configuration, run the installed binary with that selector:

```sh
WT_CONFIG=/absolute/project.toml wt state migrate --keep-legacy
WT_CONFIG=/absolute/project.toml wt ls --json
WT_CONFIG=/absolute/project.toml wt status --all --json
```

The default legacy source is `~/.cache/wt`; use `--from /absolute/legacy-cache`
when that configuration stored its JSON elsewhere. Check imported status,
sections, issue identities, fork bases, and retained sessions before continuing.
One process still selects one configuration. Remote worker state is migrated on
its host with its worker configuration, using the matching provisioned runtime
or an installed native launcher. Native and TypeScript processes do not share
live writes between the old JSON and new SQLite stores.

Reinstall an enabled events service with `wt events install` under its owning
configuration so it pins the stable native launcher and absolute config paths.
Keep the migration backups. Native `wt rollback` selects a previously installed
native release; returning to TypeScript instead requires the preserved checkout
and legacy data. It does not automatically export later SQLite edits to JSON.

## Remote workers

The controller and worker speak a versioned native protocol. A controller
provisions the matching native runtime before sending commands. Same-platform
workers can receive the verified running binary; cross-platform workers require
an exact published release for the controller's build ID and the worker's
target. The controller stores the worker executable under a content-addressed
path and validates its SHA-256, boot-probe identity, and worker handshake before
using it. Remote commands do not update, replace, or require a source checkout
on the worker.

See [configuration.md](configuration.md#remote--optional-ssh-worktree-host)
for worker setup and [backends.md](backends.md) for worktree storage behavior.

A custom local build can lack a matching binary for a remote platform. If
release lookup returns GitHub 404, or the release has no required artifact,
remote session entry stops as soon as that failure is known. The board shows
“Cannot open remote session” with Dismiss (Enter or Esc) and Copy details (`c`).
The details include the local build and platform, remote platform, release,
and GitHub error. Install a published release or supply a matching worker build.
Network errors, invalid checksums, and build mismatches remain distinct errors.
