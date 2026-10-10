# Development

## Local Rust checks

`cargo xtask gate` is the complete local gate. It reports Cargo target size,
checks formatting, runs workspace Clippy with warnings denied, enforces crate
dependency direction and workspace dependency inheritance, runs tests, then
runs doctests. It uses `cargo-nextest` when installed and falls back to Cargo's
test runner otherwise. `cargo xtask gate --package wt-store` runs the same
checks for one package; that result is explicitly package-scoped and does not
claim that the workspace is green. Formatting and dependency-policy checks
remain workspace-wide in both modes. Package-scoped gates skip doctests for a
binary-only package and say so explicitly.

`cargo xtask deps-check` checks that dependencies declared by the workspace are
inherited with `workspace = true`. `wt-core`, `wt-platform`, and `wt-store`
remain independent foundation crates; `wt-config` may use the pure domain
types in `wt-core`. `wt-runtime` only depends on those foundations. `wt-tui`
may depend on `wt-core` and `wt-runtime`, and cannot import service adapters
that would move I/O into rendering or input handling.

`cargo xtask hygiene` reports the combined size of the workspace `target/` and
an external `CARGO_TARGET_DIR`. The 10 GiB threshold is a soft warning. The
report does not remove anything and gate never cleans automatically. For explicit
cleanup, `cargo xtask hygiene --clean-incremental` acquires exclusive Cargo
profile locks and removes only old incremental compiler state, stopping when the
workspace target falls below the threshold. It refuses if a profile lock is held
or if the workspace `target/` is a symlink. External targets are measured but
never cleaned. If incremental state is insufficient, the target may remain over
the soft limit; use a separately reviewed, Cargo-aware cleanup for other build
artifacts. Build concurrency defaults to four jobs (`CARGO_BUILD_JOBS` overrides
it), and nextest defaults to four test processes.

The workspace's development profile keeps line tables for project code and
disables dependency debug symbols to reduce disk use while retaining useful
backtraces. Do not replace the soft size report with an automatic `cargo clean`
or a broad artifact sweep.

## CI and release checks

`rust-ci.yml` runs formatting, Clippy, dependency policy, packaging helper tests,
and native unit/integration tests on Linux and macOS. Lint, test, and optimized
builds have separate Cargo caches; CI disables incremental compilation. Pull
requests run this workflow directly. On `main`, `release.yml` calls the same
checks while building the four release targets in parallel, then publishes only
after every check and package succeeds. This avoids a duplicate main-branch test
run and removes the former Bun workflow. Discord digests and failure alerts
follow completion of the encompassing native release workflow.

`scripts/fixture.sh build` creates a native 24-worktree board with sections,
stacks, statuses and cleanup hazards. Build `wt-app` first, or set `WT_NATIVE_BIN`
to the binary to exercise. `scripts/fixture.sh probe` opens it in a private tmux
session; `scripts/fixture.sh rm` removes that fixture. `scripts/tui-test.sh`
also supports explicit native binary probes; without the fixture's isolated
configuration, treat a probe as read-only.
