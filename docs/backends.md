# Worktree backends

A **backend** decides how wt materializes an isolated branch checkout on
disk. It's the local-materialization axis, selected by `[backend] kind`
(see [configuration.md](configuration.md)). Two built-ins:

| backend | mechanism | object db | discovery |
|---|---|---|---|
| `git-worktree` (default) | `git worktree add/remove` | one shared db with the main clone | `git worktree list --porcelain` |
| `rift` | copy-on-write clone ([`rift`](https://github.com/anomalyco/rift)) | **independent** `.git` per checkout | scan the worktree root for `.rift` markers |

The lifecycle service selects the backend for create and remove operations.
Fork-base records, configured file copies, `.sst/stage` pinning, upstream
wiring, locks, and worktree status are handled by the native lifecycle and VCS
crates. The backend selection is defined in `crates/wt-config` and implemented
by `crates/wt-lifecycle`.

## Why rift

`rift create` copy-on-write-clones the whole working tree (APFS
`clonefile` on macOS, btrfs snapshots / reflinks on Linux). File data is
shared until modified, although creating file metadata still takes time
on large trees. With `--copy-all` it brings project files across without a package install,
so any dependencies already present in the main clone are present immediately.
wt passes `--copy-all` to `rift create`.

wt resolves the `rift` executable from the process `PATH`, then asks the user's
login shell when it is missing. The fallback is bounded to ten seconds and
only discovers an executable path; create/remove still use exact arguments.
This supports editor, daemon and agent launches whose PATH lacks shell setup.

Rift copies the project tree from the main clone. Dependencies are present
when they exist in that tree, but wt does not synchronize package installs as
part of the native worktree lifecycle. A `.rift.toml` postcreate hook can run
project-specific setup when required.

## Setup

The `rift` binary must be available on the process or login-shell PATH, and the main clone must be
rift-registered (`rift init`). wt runs `rift init`
**lazily** on the first create (idempotent, guarded on the `.rift`
marker) rather than at startup, so launching wt never pays a rift
subprocess. If `rift` isn't installed, create/remove fail with a clear
message pointing at the install command or back at `git-worktree` —
there's no startup pre-check.

## The independent-clone model

A rift checkout is a full, independent clone: its own `.git` directory,
its own object db and refs, detached at the main clone's HEAD, then
switched onto the target branch. That switch runs with
`--discard-changes`: `--copy-all` copies the main clone's working tree
INCLUDING any uncommitted modifications, and a plain switch would
refuse whenever those files differ across the jump — which used to
abort creation any time the main clone was dirty. The dirt exists only
in the throwaway copy (the main clone is never touched), so discarding
it is safe and removes the clean-main-clone requirement entirely.

Before switching, wt refreshes the copied Git index's file metadata.
The clone has new inodes, so using the source index unchanged makes a forced
switch rewrite files whose contents already match. Refreshing avoids that
write and watcher burst while still discarding copied local modifications.
This independent-clone model is the crucial difference from a git worktree,
and it drives the rest of the design:

- **Discovery.** A rift checkout never appears in `git worktree list`.
  `listWorktrees` scans the worktree root for immediate children carrying
  a `.rift` marker and synthesizes rows, reading the branch straight from
  `.git/HEAD` (pure fs, no subprocess per checkout). Done regardless of
  the configured backend, so existing checkouts of either kind stay
  visible after a flip. A clone under a live creation lock stays out of the
  inventory until initialization finishes, preventing status and diff queries
  from competing with checkout. Stale locks do not hide completed worktrees.
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
  it. Every GC call, including failed-create rollback and stale-registry
  cleanup, uses `nice -n 10` when available and macOS `taskpolicy -b` when
  available to yield CPU and I/O scheduling to foreground work. Cleanup
  remains awaited, with the same failure handling. Missing priority tools
  fall back independently to the remaining tools or plain GC. GC requests
  and completion are logged separately from removal; request elapsed time
  includes shared process-queue wait and execution. Cancellation is logged
  as interruption after the child is stopped and joined, not cleanup failure.
  Branch deletion is moot — the branch vanishes with the clone. The
  fork-base reparenting of *dependents* is backend-agnostic (it edits
  wtstate) and still runs.
  **`force` is advisory here, so callers own the dirty guard.** wt passes
  `--force` through for symmetry with the git backend, but rift trashes a
  dirty checkout with or without it — there is no refusal to rely on. That
  makes the asymmetry between the backends dangerous rather than merely
  untidy: `git worktree remove` refuses a dirty tree on its own, so a
  caller that forgot the guard still fails safe under `git-worktree` and
  destroys data under `rift`. And rift loses strictly more: a linked git
  worktree's branch and objects live in the shared main-clone database and
  survive the directory, while a rift clone owns its own, so removal takes
  the branch, the objects and the reflog with it — there is nothing to
  recover from. Every destroy path checks dirtiness itself before calling remove; keep that
  guard in `crates/wt-lifecycle` and the CLI cleanup planning path.
- **Restacking.** `R` / `wt restack` replays each slice in its own
  worktree. A rift slice can't see a sibling slice's branch as a LOCAL ref
  (separate object stores), so the engine resolves a parent through the
  `origin/<parent>` remote-tracking ref every clone carries instead of the
  bare branch name (`crates/wt-stack/src/replay.rs`) — it
  prefers the local branch when present (git-worktree, unpushed-safe) and
  falls back to `origin/<parent>`, so it works with rift's own ref layout
  rather than around it. For the Pass-2 rebase target — the parent's
  just-replayed tip, a commit that lives only in the parent's clone — the
  engine brings it over with a local object fetch from the parent's
  worktree into the clone's `origin/<parent>` remote-tracking ref (which
  the parent just force-pushed, so the ref should mirror it) — NOT a
  `refs/heads/<parent>` local branch, since creating those leaves stale
  refs that later read as phantom conflicts. Gated on the commit being
  absent so the git-worktree path never fetches. Both compose across a
  MIXED chain (a plain linked worktree stacked on a rift clone, or vice
  versa — common right after flipping the backend on). `wt restack
  prune-backups` also sweeps each rift slice's own clone, since backups are
  created per-clone.
- **Base resolution for reads.** The conflict-probe glyph and the diff /
  sync counts resolve a stacked slice's base through the shared stack-base resolver:
  the local branch if present (git-worktree), else `origin/<parent>` (the
  only ref a rift clone has for a sibling), else trunk. Keeping a rift
  clone's `origin/<parent>` fresh (the restack does this on replay; it's
  set at clone time otherwise) is what keeps that probe honest — against a
  stale ref a just-restacked rift slice reads as a phantom conflict.
- **Detection, not storage.** Which backend owns a checkout is derived
  from disk (`.rift` marker → rift) at removal time, never persisted. Flip
  `kind` freely; each checkout is torn down by whatever created it.
- **AI-session trust.** Claude Code and Codex treat each independent clone
  as a brand-new project and show their "trust this folder?" gate (which
  also suppresses the worktree's harness allow rules) — a git worktree
  sidesteps this because it resolves to the already-trusted main repo. So
  before spawning a session in a rift checkout, wt marks the path trusted
  via the harness's optional `ensureTrusted` hook: Claude in `~/.claude.json`
  (`.projects["<path>"].hasTrustDialogAccepted = true`,
  `crates/wt-harness/src/claude/`), Codex in `$CODEX_HOME/config.toml`
  (`[projects."<path>"] trust_level = "trusted"`,
  `crates/wt-harness/src/codex/`). Idempotent and best-effort; skipped once
  already trusted. The Codex entry lives in a tracked (stowed) config, so
  it's removed on teardown — Claude's `~/.claude.json` is its own churny,
  untracked file and is left. OpenCode has no such gate.

  **The Claude write is verified, and repairs its siblings.** `~/.claude.json`
  is one shared blob that Claude Code also read-modify-writes, and its window
  between reading at startup and flushing its snapshot back is *seconds*
  against the ~2.3ms wt spends — so a wt write can be complete, atomic, and
  gone. The trust writer therefore reads back after the rename and re-applies
  (3 attempts, 60/200ms), and reports an attention warning when it still could not make it stick, because the next thing the user sees is the dialog. It also
  re-asserts every rift sibling under the same worktree root that Claude already
  has an entry for: a stale flush wipes a whole snapshot rather than one key, so
  a per-path seed heals one worktree per spawn and leaves the rest — six starts
  inside 90 seconds cost three hand-accepted dialogs. Repairing siblings makes
  the Nth start heal the first N-1, so a batch converges. Siblings are repaired,
  never introduced (entry must already exist), and the set is derived from disk,
  so there is no list to maintain.

## Stale remote-tracking refs

A rift clone's `origin/<trunk>` is frozen at clone time. `fetchOrigin`
runs `git fetch` in the **main clone** only, so nothing inside a
worktree advances its copy — historical measurement on a live fleet of 28 rows,
`git rev-parse origin/staging` gave 10 distinct answers, one of them 9
merges behind and none of them the tip. Two rules fall out of that, and
both have shipped as bugs:

**Fix the REF, not each reader.** `fetchOrigin` ends by pointing every
worktree's own `origin/<trunk>` at the tip the main clone just fetched
(`freshenWorktreeTrunkRefs`). Everything keyed to the base reads that
ref: the ahead/behind counts, the pre-PR row title (the oldest commit
in `base..HEAD`, which otherwise becomes a colleague's commit), the
diff context the AI summary is generated from, the git row's
files/insertions, the merge-conflict probe (which reports clean against
a trunk several merges old, a false green), the `{{base}}` handed to
the diff tool — and the agent's own `git log origin/<trunk>..HEAD`
inside the checkout, which is the reader no wt-side substitution can
reach. Historical measurement before the fix: a branch with no commits of its own
showed 3 ahead and an 11-file diff of somebody else's work; a real
branch showed 304 files changed against a true 24; 17 of 18 checkouts
were stale across three generations.

It is cheap in the shape it runs. The value comparison exits first,
which covers every checkout under `git-worktree` (one shared ref store)
and every already-current rift clone, and the object is nearly always
present already, so it is a ref write rather than a transfer (0 of 19
live checkouts needed the fetch fallback). Steady-state cost is ~75ms
for 18 checkouts against a `git fetch origin --prune` that already
costs 1.6-2.3s. **Fast-forward only**: a clone runs its own `git fetch`
too and can be ahead of the main clone's last one, and rewinding its
ref is the same lie pointing the other way.

**The trunk has two spellings and only one of them was normalized.**
A fork-base record is written for trunk forks too, storing the BARE
branch name (a recorded fork-base branch of `staging`). the resolver recognized
`origin/staging` and fell through on `staging` — into a preference for a
LOCAL branch of that name, which exists in every rift clone (`clone -b
staging` leaves one) and which nothing ever moves: the freshen above
fast-forwards `refs/remotes/origin/<trunk>`, not the local head. So an
ordinary trunk worktree measured itself against clone time, permanently,
and the fallback that was supposed to catch this could not: `freshBaseRev`
keys on the trunk string as well, saw a base that was not it, and
returned it untouched. Historical measurement on a live fleet of 15: every row resolved
to a local trunk 97 to 383 commits behind, one reporting 383 commits
ahead against a true 1. Nine rows shared a single colleague's commit
subject as their title, because that commit was the oldest in all nine
ranges. Both spellings now normalize.

**Resolve a ref in the frame the question is about.** "Where is trunk
now" resolves in the main clone (`baseTipSha`); "what does this checkout
see" resolves in the worktree. A comparison that spans both needs the
same helper on each side, or it compares reference frames rather than
commits. Sync counts (`syncState`, `pushCounts`) and the pre-PR row title
(`wtFirstCommitQuery`) additionally go through
`freshBaseRev`, which swaps the trunk ref for the main clone's SHA when
the checkout already holds that object — a floor under the freshen
above, for the window between a merge landing and the next fetch. It
counts, it never fetches, and anything that is not the trunk ref (a
stacked parent, an external base) is left alone, because there the
local copy genuinely is the freshest. That number is not cosmetic —
before the branch is pushed it is also `pushCounts().unpushed`, which
is what the destroy guards read, so an empty worktree claimed to be
holding work at risk.

**"Do I have the object" is not "is my ref current".** The restack's
trunk-root freshen (`resolveNewBaseSha`) used object presence as its
gate, and the object routinely arrives early by another name: a plain
`git fetch origin` in the clone pulls GitHub's merge-queue branches
(`gh-readonly-queue/<base>/pr-N-<sha>`), whose tip *is* the commit that
becomes trunk minutes later. The gate read "already fresh", the ref
never moved, and the rebase landed on the right commit while every later
reader in that clone — counts, the conflict probe, the agent's own
`git log origin/<trunk>..HEAD` — kept measuring against a tip several
merges behind. It compares ref VALUES now.

Neither is reproducible in `scripts/fixture.sh`, which writes no
`[backend]` section and therefore runs `git-worktree`, where the ref
store is shared and every clone's answer is the same answer. See
`crates/wt-vcs/src/origin.rs` for origin and clone behavior.

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
Keep backend selection in the lifecycle service and the `wt-vcs` backend
implementation; do not spread backend branching through UI actions.

## Known limitations (rift)

- macOS APFS or Linux btrfs/reflink filesystems only (rift's constraint).
- **Postcreate hooks see the clone-time HEAD, not the target branch.**
  `.rift.toml` hooks run inside `rift create`, before wt switches onto the
  branch — so the working tree is at the main clone's commit (detached).
  Fine for lockfile-sync hooks (a package-manager install); a branch-name-sensitive
  hook won't see the final branch.
- **`--keep-branch` can't preserve an unpushed rift branch.** A rift
  branch lives only in the clone's `.git`; removing the checkout destroys
  it regardless of the flag. A pushed branch survives on origin either way.
- Each checkout duplicates the object db (cheap on disk via CoW, but the
  refs are not shared): a sibling slice's branch isn't a local ref in a
  rift checkout. The restack engine handles this by resolving parents
  through the shared `origin/<parent>` refs and a local object fetch for
  the just-replayed tip (see **Restacking** above), and push works
  per-clone. The
  mid-rebase conflict glyph is push-based for rift too: a per-clone watcher
  on each rift slice's own `.git/rebase-*` (`RiftRebaseWatchSet`) fires the
  conflict probe, the independent-clone analogue of the linked-worktree
  `.git/worktrees/<slug>/rebase-*` watcher. Other ad-hoc cross-checkout ref
  reads *outside* these paths still don't propagate — e.g. a sibling's
  just-merged/pushed state can lag a rift row until its staleTime.
- `--copy-all` also brings other regenerable artifacts (`dist`, `.turbo`,
  caches) across via CoW — harmless for a fresh identical checkout, and
  free.
