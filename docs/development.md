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
