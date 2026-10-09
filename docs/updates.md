# Updates, rollback & compatibility

Native installs use GitHub Releases and immutable per-build directories; they
do not fetch or modify a source checkout. The stable launcher path remains
`~/.local/share/wt/bin/wt` unless the installer supplies `WT_INSTALL_ROOT`.
Release packaging and the CI manifest contract are described in
[the distribution design](rust-distribution-design.md). The earlier source
updater remains only as a behavior reference during the Rust rewrite; see
[the rewrite record](rust-rewrite.md).

SSH worker commands use separate source packages prepared by the controller
(see [configuration.md](configuration.md#remote--optional-ssh-worktree-host)).
These packages have no Git metadata. Their `.wt-runtime.json` records the
controller build for `wt version` and the worker handshake. Source-clone
updates and rollback still use Git. Automatic runtime setup does not replace
the worker's source clone or remove packages used by existing sessions.

Stable follows the latest non-prerelease GitHub release; preview follows the
latest `preview-<fullsha>` prerelease. `wt update --channel stable|preview`
persists an explicit channel choice, while the default uses the saved channel.
`wt update --check` reads metadata without downloading. For explicit release
artifact testing, `wt update --release <tag>` selects that exact GitHub release,
including a `rust-test-*` tag; this does not change automatic channel discovery.
The legacy `--head` option is rejected because native updates only accept
CI-published manifests.
`wt update log` and `wt rollback [release-or-sha]` use the same install state.

## The moving parts

- **Version identity** keeps release tag, full build SHA and target separate.
  Each identity has an immutable directory under `versions/`; state records
  current, last-good and pending boot identities.
- **Memory** is `<install-root>/state.json`: saved channel, daily check time,
  declined build, boot transition and bounded operation history. Unknown fields
  survive read/write so newer launchers retain policy state.
- Update and rollback commands run before repository config and database load.
  They remain usable when repository configuration is broken and never need a
  source checkout.
- OS file locks serialize state transitions across launcher, app, update and
  rollback processes. Downloads and extraction happen outside the short state
  lock; activation uses one atomic state-file replacement.

## Prevent: release manifests and the boot probe

Each stable or preview release must include `wt-release.json`. The client
matches the manifest's full build SHA, archive name, size and digest against
the GitHub release, then checks the archive's `wt-build-info.json` before using
its two executables. Metadata and downloads have strict size limits; extraction
rejects links, traversal, extra files and unexpected entry types. Preview only
offers `preview-<fullsha>` tags. The separate `rust-test-*` workflow tags are
never candidates for automatic preview updates. They can only be selected by
an intentional `wt update --release rust-test-…` invocation.

The stable launcher runs `--_boot-probe` without user arguments and verifies
the candidate's compiled build and target before dispatching the real command.
A failed pending probe selects the fallback before the command has started.
After application initialization succeeds, wt confirms the exact release and
attempt token. Explicit early startup failure restores its fallback. Once a real
command starts, its exit status never triggers rollback or argument replay.

The startup check runs at most once per day. `[update] startup_check = false`
and `WT_UPDATE=off` disable it. A declined build stays suppressed until a newer
build appears; explicit `wt update` is the deliberate reapply path.

## Detect: the boot attempt

The launcher records a pending build and fallback before dispatch. Confirmation
clears that marker and promotes the candidate to last-good. If a process dies
before confirming, the next launcher probe can reject the candidate without
loading repository config or replaying user arguments. Confirmation, explicit
startup failure and launcher rejection all compare release identity and the
unique attempt token under the durable state lock.

## Recover: rollback

`wt rollback [<release-or-sha>]` selects the pending fallback, last-good or
previous confirmed identity from local history. It probes the chosen installed
binary before activation, then records a pending rollback and declines the
build being left behind in one state write. A bad candidate leaves the active
version unchanged. The stable launcher still probes again on the next start.
`wt update log` prints local update and rollback history and current, last-good
and declined build identities.

## Evolve: data compatibility across hot updates

Stores have explicit compatibility policies:

- **`~/.local/state/wt/wt.sqlite`** (fork bases, controller-owned local/remote sections, work statuses,
  archives and removed history — durable, not rebuildable): one database for
  the machine, with every row scoped by a path-derived `repo_id`. SQL schema
  changes use the forward-only `schema_migrations` ledger in
  `core/state-db.ts`. The repository-state payload retains its existing
  forward-only `WT_STATE_VERSION` transformations, so the proven migration
  helpers remain the compatibility boundary while storage evolves. The
  v17 payload adds an optional trimmed `manualTitle` and its monotonic
  `manualTitleRevision` on worktree slug records; existing titles are not
  inferred from cached AI summaries. The
  current-schema read path opens the existing database read-only and does not
  refresh repository timestamps; only writes and a pending schema migration
  need write access. This keeps `wt status` and `wt fleet` usable from a
  restricted agent sandbox. The `repo_id` that scopes every row is derived
  from the REPOSITORY, never from the working directory a command ran in — see
  [configuration.md](configuration.md#repository-identity-is-a-property-of-the-repository-not-of-your-shell).
  A build that got that wrong does not corrupt anything, it PARTITIONS: each
  namespace stays internally consistent and simply cannot see the others, which
  reads as data loss from every vantage point at once. `wt state migrate`
  adopts stranded namespaces back, and is the pattern any future change to the
  id must ship with — a source fix cannot heal state an earlier build already
  filed elsewhere.
- **`cache.sqlite`** (persisted queries): `CACHE_BUSTER` in
  `src/state/client.ts` advances on shape or meaning changes. Incompatible
  entries are discarded. The v32-to-v33 title consolidation carries forward
  valid AI summaries on read, dropping only the obsolete `brief` field:
  manually requested names must survive without another model call. Both
  hash-keyed and per-slug summaries keep their original title, description,
  and expiry; unrelated entries still bust. There is no disk rewrite.
- **`communication-holds.json` beside the state database** stores the latest
  resource event. Version 1 is parsed strictly; malformed or newer formats fail
  without rewriting. Release watermarks are not a disposable cache. Hold
  deadlines bound only transient holds, not other durable state. Older builds
  leave this separate file untouched.
- **User config** (hand-written TOML): never rewritten by wt. Renames
  get loader aliases plus a deprecation warning (the
  `TRIGGER_ALIASES` pattern in `core/config.ts`); new fields get
  defaults or a fail-fast error with a copy-pasteable snippet. A
  config that loaded yesterday must load today.

`wt state migrate` is the boundary from the former shared JSON store. It
selects only records attributable to the current repository, imports them in
one SQLite transaction, backs up the source files, and removes only the rows
successfully imported. The command is idempotent and current SQLite values
win, so `--keep-legacy` is available for a copy-only first pass.

A rollback to a pre-SQLite wt build cannot corrupt the database because that
build does not know it exists; it will continue writing the legacy JSON files.
Those post-migration legacy writes are intentionally not merged
automatically. Restart long-lived wt processes together when crossing this
storage boundary, and retain the migration backup until the new build has
been exercised.

## Escape hatches

`[update] startup_check = false` disables the daily startup offer;
`WT_UPDATE=off` disables the entire update system (check, sentinel,
offers) for one run — the probe harness arms it. Everything the
automation does is also just git: `git -C ~/.wt log|reset|pull` remain
the ultimate manual override.
