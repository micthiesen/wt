# SST stages

`wt stages` reads SST's Pulumi state objects from the configured S3 bucket using
the AWS CLI and the configured profile. It only considers stage names under
`[stage].prefix`; the configured `default_personal` stage is always protected.
Names referenced by a live non-main worktree are listed as live. A remaining
stage is an orphan only when its state file parses and contains resources.
Empty state files are omitted because `sst remove` leaves them behind.

AWS errors and malformed state are reported in an `unknown` group. They are not
treated as empty, and are never eligible for automated deletion. `--clean`
destroys only verified orphan stages, through `pnpm sst remove --stage <name>`
in the configured main clone. It requires an interactive confirmation or
`--yes` / `-y` in a non-interactive shell. Before each deletion, wt reads the
worktree inventory again and skips any stage that has become live since the
initial listing or confirmation. A partial cleanup exits nonzero if a stage
state was unknown, a candidate became live, or an external remove failed.

`--json` emits `{ "live": [...], "orphaned": [...], "unknown": [...] }`;
each row has `name`, `size_bytes`, and `modified`, and unknown rows also have
`reason`. With `--clean --json`, the snapshot remains the only stdout output;
cleanup progress and errors go to stderr. This is a snapshot, not a lock on S3:
a deployment can race after the final inventory check. Keep the normal SST
project permissions and deployment coordination in place when cleaning stages.

The Rust `wt-sst` crate also owns local stage safety helpers used by application
surfaces. A local deployment is reported only when `.sst/stage` contains a
valid name under the configured prefix and valid `.sst/outputs.json` references
that exact pin. Missing data means not deployed; unreadable or malformed data
means unknown. Destructive consumers must proceed only from a positive owned
stage fact and still revalidate their own action's live inventory.

The application itself is a native binary and does not depend on Bun or a
source checkout. SST remains an external project integration: `aws` supplies
S3 access and the project's installed `pnpm`/SST toolchain performs stage
removal. Test the CLI and cleanup guards without cloud access using:

```sh
cargo build -p wt-app --locked
python3 scripts/native-stages-check.py --binary target/debug/wt
```

The check creates private Git worktrees and puts fake `aws` and `pnpm`
executables first on its temporary `PATH`. It does not contact AWS or invoke a
real SST deployment.
