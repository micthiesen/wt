# CI, build caching and releases

How wt's GitHub Actions are arranged to stay fast, and the invariants that keep
them fast. Most of these were learned by measuring a regression; each one names
what breaks if it is undone. Change a workflow and this file together.

The repository is public, so GitHub-hosted runners are free. The standard
runners are what we use: macOS (`macos-15`, `macos-latest`) has 3 cores, Linux
x86 and Linux arm (`ubuntu-22.04-arm`, free for public repositories) have 4.

## Workflows

| Workflow | Trigger | What it does |
|---|---|---|
| `release.yml` (Native release) | push to `main`, push of a `v*` or `rust-test-*` tag, manual dispatch | Calls `rust-ci.yml` for lint, unit and native checks. Only for a tag or a dispatch does it also build the three release packages and publish. |
| `rust-ci.yml` (Rust CI) | called by `release.yml`, pull requests | `lint`, `test` and `native` on Linux and macOS. |
| `release-cache.yml` (Release cache) | push to `main` | Compiles the release binaries only to keep `main`'s optimized cache warm for tags. |
| `discord-ci-alert.yml` | completion of Native release or Release cache on `main` | Posts unsuccessful, non-cancelled runs to #github. |
| `discord-release.yml` | completion of a tag's Native release run | Posts the stable release and its notes to #updates ([discord.md](discord.md)). |

A `main` push therefore reports its checks in about the time of the slowest
check job, while the release-cache build runs separately. A preview release
of `main` is published only on demand, by a manual dispatch with
`publish=true`. Releasing is
documented in [updates.md](updates.md#contributor-validation-and-release-gate).

## Caching

Every Rust job uses `Swatinem/rust-cache`. Together these settings let a job
restore `main`'s compiled output and recompile only what changed:

1. **Only `main` saves caches** (`save-if: github.ref == 'refs/heads/main'`).
   GitHub scopes a cache to the ref that saved it: a tag or branch run can
   restore caches saved by its own ref or by the default branch, nothing else.
   A save from a tag, a pull request or a feature branch can never be reused,
   and before this rule such saves filled most of the 10 GB repository quota
   with duplicate keys.
2. **Tags restore what `main` built.** `release.yml`'s package job has
   `save-if: false` and restores the `native-release-<target>` cache that
   `release-cache.yml` saves on `main`. Both must build the same packages
   (`-p wt-app -p wt-launcher --release --locked`) with the same toolchain,
   `CARGO_*` environment and `shared-key`. Otherwise the tag restores output it
   cannot use and compiles from scratch, which is correct but slow.
3. **Workspace crates are cached** (`cache-workspace-crates: true`). By default
   the action keeps only third-party dependencies, and wt's own crates are most
   of the compile time.
4. **Files are dated by their last commit** (`scripts/restore-mtimes.py` after
   a `fetch-depth: 0` checkout). Cargo treats a workspace crate as stale when a
   source file is newer than its cached output, and a checkout stamps every
   file with the current time. Without this step, item 3 does nothing:
   every cached workspace crate rebuilds. The script fails rather than guess
   when history is shallow.
5. **`test` and `native` share one debug cache per OS** (`debug-<os>`). Both
   build with the dev profile, so their dependency output is identical; two
   keys only doubled the cache size.

Cache keys include a hash of every `Cargo.toml` and `Cargo.lock`, so a version
bump produces a new key. The action then restores the newest cache with the
same prefix, which is the intended fallback, and saves the new key from
`main`. Old keys age out under GitHub's least-recently-used eviction.

## Versions

Only `wt-app` and `wt-launcher` carry the release version
(`version.workspace = true`). Every library crate has the fixed version
`0.0.0` and `publish = false`. Cargo hashes a crate's version into its
identity, so when every crate inherited the workspace version, each release
bump recompiled all of them. Now a bump recompiles only the two binaries, and
the libraries come from `main`'s cache. Code that needs the release version or
build identity reads it in `wt-app` (`CARGO_PKG_VERSION`, and `WT_BUILD_ID`
through `crates/wt-app/build.rs`) and passes it down. Never read either in a
library crate: it would make that crate, and everything above it, rebuild for
every release or commit.

## Native end-to-end checks

`scripts/native-check.py` runs the native suite. CI sets `WT_NATIVE_JOBS` to
one check per runner core. Each check owns its own temporary home, sockets and
tmux server, so checks can share a machine. The Linux workload check measures
CPU and memory limits, so it is marked exclusive and runs last, alone. A new
check that uses a fixed path, port or global service must be made isolated or
marked exclusive.

## Concurrency

`release.yml` cancels a superseded run on the same branch
(`cancel-in-progress` for branch refs) but never a tag run, which is a
publication. `release-cache.yml` keeps only the newest `main` run.

## Measured baseline

Release v0.1.34, before this arrangement: macOS package 8.6 minutes (7 of
them compiling `wt-app`), macOS native checks 5.9, macOS unit tests 4.1, three
full runs per release (verification branch, `main` and tag).
