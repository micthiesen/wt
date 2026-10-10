# Updates, rollback, and stored data

Installed wt releases are native binaries fetched from GitHub Releases. Updates
do not pull or modify a source checkout. The installer and updater verify the
release manifest, archive checksum, build identity, and target before activation.
The stable launcher lives under `~/.local/share/wt` by default; set
`WT_INSTALL_ROOT` to select another install root. The release manifest and
supported targets are documented in [the distribution design](rust-distribution-design.md).

Stable follows the latest non-prerelease release. Preview follows the latest
`preview-<fullsha>` prerelease. `wt update --channel stable|preview` persists a
channel choice; otherwise the saved channel is used. `wt update --check` checks
metadata without installing. `wt update --release <tag>` selects one exact
release for testing, including `rust-test-*` tags. Those test tags are never
selected by normal stable or preview discovery. `wt update log` prints local
update history. The old `--head` source-update option is rejected.

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
token. If the pending binary fails before confirmation, the launcher selects
the prior fallback on the next launch. Once an ordinary command has started,
its exit status does not trigger rollback or argument replay. Explicit early
startup failure can restore the fallback immediately.

Update-state transitions use an OS file lock and atomic state-file replacement.
Downloads and archive validation happen before activation. Extraction rejects
path traversal, links, unexpected files, and mismatched size or digest. The
candidate is placed in its immutable directory before state points at it, so a
crash may leave an unused version directory but cannot select incomplete files.

The startup offer checks at most once per day. `[update] startup_check = false`
or `WT_UPDATE=off` disables it. A declined build remains suppressed until a
newer build appears; explicitly running `wt update` is the reapply path.

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
- **Derived caches** under the repository cache root include generated naming
  summaries, event snapshots, remote presentation snapshots, and rebuildable
  runtime data. `Ctrl+R` clears the generated naming and picker caches before
  requesting live source reads; it does not clear SQLite or accepted action and
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
