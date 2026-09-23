# Updates, rollback & compatibility

wt has no release process: `main` is the release channel, and installs
are git clones that fast-forward (see [cli.md](cli.md#wt-update-log---check---head)
for command surface). What makes that safe is not a version scheme but
four layers around the pull — prevent, detect, recover, evolve. This
page is the semantics reference for those layers; the enforcement rules
for people (and agents) changing wt live in `AGENTS.md`.

## The moving parts

- **Version** = the source clone's git short hash (`wt version`, the
  help overlay title). `-dirty` marks local modifications.
- **Memory** = `~/.cache/wt/update.json`, machine-global (one source
  clone per machine, shared by every instance including sealed ones):
  the daily-check stamp, the per-version decline, the update/rollback
  **journal**, the **boot sentinel**, and the **last good boot** sha.
- Everything under `src/core/update/` (and the `wt rollback` command
  path) is deliberately **config-free** — no imports of `core/config.ts`,
  `proc`, `locks`, or `logger` at module load (its logging goes to a
  fixed `~/.cache/wt/logs/update.log` instead). The crash-rollback
  offer must work when the config loader is exactly what the broken
  update can't run — and so must the commands: main.ts dispatches
  `update` / `rollback` / `version` AROUND `cli/index.ts`, whose static
  command imports would otherwise pull the fail-fast loader in first.
- Update/rollback git mutations on the shared clone are serialized by a
  config-free mkdir lock (`~/.cache/wt/update-git.lock`, stale-holder
  detection by pid); a second concurrent update/rollback gets "another
  update is in progress" instead of interleaved resets.

## Prevent: the CI gate and the boot probe

**Green-main gate.** The updater doesn't target origin's raw tip: it
walks the incoming commits newest-first and targets the newest one
whose CI check run (names `ci` / `typecheck`, see
`.github/workflows/ci.yml` and `GATE_CHECK_NAMES` in
`core/update/green.ts`) concluded green. Red and still-running commits
are held back; commits with *no* matching check runs (pre-CI history),
API failures, rate limits, and non-GitHub origins all **fail open** —
the gate exists to skip known-bad pushes, never to strand anyone.
Unrelated workflows (e.g. the Discord digest) can't veto an update
because matching is by check-run name. When the pick rests on an
"unknown" verdict the CLI says so ("CI status couldn't be verified —
the gate fails open") rather than letting a network problem impersonate
a green check. `wt update --head` bypasses the gate explicitly.

The CI job runs Effect language-service diagnostics, TypeScript
typechecking, the Bun production build, and the full test suite before its
single `ci` check can turn green.

**Boot probe.** After the fast-forward (and a `bun install` when the
dependency manifest changed), the updater boot-probes the checkout in a
child process: `wt version` (the CLI chain) and an import of the full
TUI module tree — which loads `core/config.ts` and therefore also
catches "new code rejects the user's existing config", the likeliest
hot-update break. A probe failure reverts code *and* deps, declines
that version (so the daily check skips it until origin moves), and
leaves the user on what they had. A broken push therefore usually
costs its author a red X, not a user a broken install.

Every interactive startup reconciles an installed events daemon against the
freshly loaded wt build. The daemon records its build in `state.json`; a live
daemon with the same build is untouched, while a stopped, older, or unstamped
daemon is restarted through the newly checked-out `bin/wt events restart`.
This happens after the update process re-execs, rather than only in the process
that applied the update: that old process may itself predate the restart hook,
and the source clone may also have moved outside the startup updater. Failure
is visible but does not strand the user before the TUI; `wt events restart` is
the manual retry. Restart readiness is established by the new process state;
its warm-up GitHub fetch can finish a moment later, so the TUI refuses the
leftover old snapshot without reporting the already-current daemon as stale.

**What the gate does not do is tell anyone it is holding.** A red `main`
stops shipping silently: users stay on their last green version, which is
the correct behaviour and produces no message anywhere. `main` was red from
2026-08-24 to 08-26 and the only visible symptom was the Discord #updates
channel going quiet, because that digest is gated on the same green run
(see [discord.md](discord.md)). When updates seem to have stopped, check
whether `main` is green before looking at `core/update/`.

**Lazy command dispatch.** For the pushes that do get through, the CLI
limits how far one broken module reaches: `cli/index.ts` imports each
subcommand's module graph on demand, so `wt status` doesn't load the
message transport and survives a break in it. It matters most for the
commands agents depend on to report trouble — a fleet that can't run
`wt status` or `wt manager report` can't tell anyone the update went
bad. `scripts/broken-module-check.sh` asserts the containment; see
[architecture.md](architecture.md#module-layout-conventions).

## Detect: the boot sentinel

Starting the TUI writes `booting: {sha, at}` to the memory; the sha is
promoted to `lastGoodSha` (and the sentinel cleared) after 15 s alive
or a clean quit, whichever comes first. The promotion is an Effect fiber
scoped to the TUI lifetime, so failure interrupts it before the crash
handler can wait at the rollback prompt. An update additionally writes
an `applying: {fromSha, toSha}` marker before its merge moves HEAD —
if the process dies mid-update (the deps/probe window runs seconds to
minutes), the offers treat the marker like a journal entry, so even an
interrupted update leaves a rollback target. Two detectors hang off
the sentinel:

- **Crash offer** — the top-level catch in `main.ts`: if the process
  dies while HEAD is a journaled update that never booted good, offer
  a rollback (default **yes** — the crash is proven).
- **Stale-sentinel offer** — pre-TUI at the next launch: a leftover
  sentinel for the current HEAD means the previous start never
  finished (native crash, kill). Weaker evidence, so the offer
  defaults to **no**, and a subsequent healthy boot clears the
  suspicion.

Both offers (and sentinel writes) are disabled by `WT_UPDATE=off`, so
probe-harness instances can't leave false crash evidence.

## Recover: rollback

`wt rollback [<ref>]` resets the clone to the last version that booted
healthy (or an explicit ref), syncs deps across the jump, journals the
move, and — the piece that makes it stick — records the abandoned sha
as **declined**, so the startup check won't re-offer the known-bad
version. The moment origin moves past it (presumably the fix), offers
resume. `wt update` explicitly can always re-apply anything. Rollback
refuses dirty/ahead clones for the same reason update does: local
divergence means a human is driving.

`wt update log` prints the journal (updates and rollbacks, newest
first) plus current / last-good / skipped shas.

## Evolve: data compatibility across hot updates

Three stores, three policies:

- **`~/.local/state/wt/wt.sqlite`** (fork bases, controller-owned local/remote sections, work statuses,
  archives and removed history — durable, not rebuildable): one database for
  the machine, with every row scoped by a path-derived `repo_id`. SQL schema
  changes use the forward-only `schema_migrations` ledger in
  `core/state-db.ts`. The repository-state payload retains its existing
  forward-only `WT_STATE_VERSION` transformations, so the proven migration
  helpers remain the compatibility boundary while storage evolves. The
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
- **`cache.sqlite`** (persisted queries — fully rebuildable): no
  migrations, ever. `CACHE_BUSTER` in `src/state/client.ts` busts the
  whole persisted cache on any shape change; busting is the *correct*
  policy for this store, formalized.
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
