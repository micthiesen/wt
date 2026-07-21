# Worktree backends

A **backend** decides how wt materializes an isolated branch checkout on
disk. It's the local-materialization axis, selected by `[backend] kind`
(see [configuration.md](configuration.md)). Two built-ins:

| backend | mechanism | object db | discovery |
|---|---|---|---|
| `git-worktree` (default) | `git worktree add/remove` | one shared db with the main clone | `git worktree list --porcelain` |
| `rift` | copy-on-write clone ([`rift`](https://github.com/anomalyco/rift)) | **independent** `.git` per checkout | scan the worktree root for `.rift` markers |

The seam is deliberately narrow: `create` and `remove` are the only two
filesystem mutation points (`core/backend/`, behind the `core/backend.ts`
barrel). Everything else wt does to a worktree — the fork-base record,
`.env` copy, `.sst/stage` pin, upstream wiring, lock, dirty/merged/gone
status — is backend-agnostic and lives in `lifecycle.ts` / `worktree.ts`.

## Why rift

`rift create` copy-on-write-clones the whole working tree (APFS
`clonefile` on macOS, btrfs snapshots / reflinks on Linux). It's
near-instant even on a large repo, and with `--copy-all` it brings
`node_modules` across **for free** — so a rift checkout has packages
installed the moment it exists, with no `pnpm install`. wt passes
`--copy-all` always; the `--no-install` flag (`runInstall`) is a no-op
for this backend. If you want a lockfile sync anyway, add a `.rift.toml`
postcreate hook in the main clone (`pnpm install --frozen-lockfile`) —
rift runs it, and it's fast because the packages are already present.

## Setup

The `rift` binary must be on `PATH` (`npm i -g rift-snapshot`), and the
main clone must be rift-registered (`rift init`). wt runs `rift init`
**lazily** on the first create (idempotent, guarded on the `.rift`
marker) rather than at startup, so launching wt never pays a rift
subprocess. If `rift` isn't installed, create/remove fail with a clear
message pointing at the install command or back at `git-worktree` —
there's no startup pre-check.

## The independent-clone model

A rift checkout is a full, independent clone: its own `.git` directory,
its own object db and refs, detached at the main clone's HEAD, then
switched onto the target branch. This is the crucial difference from a
git worktree, and it drives the rest of the design:

- **Discovery.** A rift checkout never appears in `git worktree list`.
  `listWorktrees` scans the worktree root for immediate children carrying
  a `.rift` marker and synthesizes rows, reading the branch straight from
  `.git/HEAD` (pure fs, no subprocess per checkout). Done regardless of
  the configured backend, so existing checkouts of either kind stay
  visible after a flip.
- **Freshness.** rift create/remove happen under the worktree root, not
  `.git/worktrees/`, so a dedicated worktree-root watcher is the push
  signal for the list (see the freshness table in
  [architecture.md](architecture.md#freshness-model)).
- **Stacking.** A stacked child forks off its parent's branch, whose
  commits live only in the parent's independent `.git` — not in the main
  clone the child is cloned from. wt fetches the base from the parent
  worktree (`git fetch <parentPath> refs/heads/<base>`, pulling the tip
  plus any unpushed ancestry) before branching. Fork-off-trunk needs no
  fetch (`origin/*` is already in the copy).
- **Removal.** `rift remove` trashes the subtree, then `rift gc` reclaims
  it. Branch deletion is moot — the branch vanishes with the clone. The
  fork-base reparenting of *dependents* is backend-agnostic (it edits
  wtstate) and still runs.
- **Detection, not storage.** Which backend owns a checkout is derived
  from disk (`.rift` marker → rift) at removal time, never persisted. Flip
  `kind` freely; each checkout is torn down by whatever created it.

## Self-healing registry

rift's registry is a global SQLite db that outlives directories. A
checkout deleted out-of-band (a hand `rm -rf`, an aborted create) leaves
a dangling record, and re-creating at the same path collides with
`UNIQUE constraint failed: rift.path`. wt catches this, runs `rift gc` to
prune the stale record (the path is already absent), and retries once —
so a manual `rm -rf` of a rift worktree doesn't wedge the next create.

## Orthogonal to remote

Backend (how a checkout is materialized *locally*) is a separate axis
from any remote/SSH-host feature (*where* a worktree lives). A remote
host runs its own wt with its own `[backend]` config; the two compose.
Keep new backend logic inside `core/backend/` and the two `lifecycle.ts`
mutation points — don't spread backend branching across the flows.

## Known limitations (rift)

- macOS APFS or Linux btrfs/reflink filesystems only (rift's constraint).
- **Postcreate hooks see the clone-time HEAD, not the target branch.**
  `.rift.toml` hooks run inside `rift create`, before wt switches onto the
  branch — so the working tree is at the main clone's commit (detached).
  Fine for lockfile-sync hooks (`pnpm install`); a branch-name-sensitive
  hook won't see the final branch.
- **`--keep-branch` can't preserve an unpushed rift branch.** A rift
  branch lives only in the clone's `.git`; removing the checkout destroys
  it regardless of the flag. A pushed branch survives on origin either way.
- Each checkout duplicates the object db (cheap on disk via CoW, but the
  refs are not shared): a `git fetch` in the main clone doesn't update a
  rift checkout's remote-tracking refs. Per-checkout git operations
  (restack replay, push, the stacking fetch above) work because each
  clone has its own remotes; cross-checkout ref propagation does not.
- `--copy-all` also brings other regenerable artifacts (`dist`, `.turbo`,
  caches) across via CoW — harmless for a fresh identical checkout, and
  free.
