<!--
Keep this file focused on non-obvious, load-bearing traps. Product behavior
belongs in docs/; update the relevant reference when a contract changes.
-->

# wt agent instructions

wt is a native Rust terminal application for managing Git worktrees. The
workspace is under `crates/`; the production binary is `wt-app`, and its
presentation model and terminal driver live in `wt-tui`.

## Working in this repository

- Follow the current task and user authorization for branches, commits,
  releases, and external writes. Do not infer a main-only workflow from old
  TypeScript-era notes.
- Read `docs/architecture.md` before changing composition, source scheduling,
  host routing, or terminal interaction. Read `docs/fleet.md` before changing
  status, manager, or automation agency.
- Keep the source of truth current: `docs/configuration.md` mirrors
  `wt-config`; `docs/tui.md` mirrors keybindings and panes; `docs/cli.md`
  mirrors Clap commands and flags. Feature semantics live in
  `docs/automations.md`, `docs/github-events.md`, `docs/stacked-prs.md`, and
  `docs/manager.md`.
- Changes to update/recovery, skills distribution, backends, or Discord
  integration also update `docs/updates.md`, `docs/skills.md`,
  `docs/backends.md`, or `docs/discord.md`, respectively. Bundled skills are
  brand-neutral because wt is OSS. Edit bundled sources in this repository and
  distribute them with `wt skills sync`; do not edit installed copies. Replace
  the managed `instructions.md` block instead of appending, and keep its
  enforced line budget.
- Use the existing Cargo workspace boundaries. `wt-config` owns selected
  configuration, `wt-store` durable state, `wt-platform` subprocesses and OS
  resources, `wt-runtime` scheduled source lifetimes, `wt-vcs` Git operations,
  `wt-lifecycle` worktree safety, `wt-harness` agent adapters, and `wt-app`
  composition. Keep pure domain rules in `wt-core` or the owning domain crate.
- Put filesystem/process work off the terminal input path. Sources publish
  prepared snapshots through `wt-runtime`; actions go through the host service
  and tracked controller. Use scoped cancellation and preserve accepted
  mutations through shutdown drains.
- Run focused Cargo tests for changed crates, then the relevant workspace gate
  when shared contracts are stable. Keep fixtures isolated from live worktrees,
  accounts, and agent sessions.

## Load-bearing contracts

- A process loads one fully merged configuration at startup. Keep repository
  identity independent of the caller's current directory; linked worktrees may
  have only a `.git` pointer and `commondir`, with configuration in the main
  clone. Tests must use isolated configs with an explicit repository identity,
  not ambient user configuration. Do not switch configuration sources midway
  through a process.
- Durable SQLite state is authoritative. Cache files, source snapshots, and
  event observations may be rebuilt; never clear status, layout, issue identity,
  fork-base records, or accepted operation history as a cache-refresh shortcut.
- Stack membership is inferred from per-worktree fork-base records. Preserve
  `baseSha` when rewriting a record; it is the squash-safe replay anchor.
  `baseBranch == trunk` means no parent. A branch is landed only when it has
  commits of its own beyond its recorded base and the required Git/GitHub proof
  succeeds.
- Cleanup never forces removal. Dirty files, unpushed commits, live operation
  locks, and outstanding `verifyAfterMerge` checks remain explicit hazards.
  Unknown inventory, GitHub, or host state is not evidence that cleanup is safe.
- Batched GitHub reads fail closed as a whole. A missing chunk is not proof that
  a branch has no PR. Retry transient transport failures at the chunk boundary;
  never turn rate-limit or ambiguous mutation results into automatic retries.
  Keep fetches batched and coalesced, with the minimum interval measured from
  the prior fetch start so slow requests count toward the floor.
- Merge-queue submission and classic auto-merge are separate operations.
  Select from the PR's base branch when arming, inspect the PR's actual state
  when cancelling, and retain the expected head SHA across retries. A lost or
  uncertain mutation reply is not permission to send it again.
- Codex message enqueue has no idempotent write. Reconcile a lost reply against
  queue and recent item state; if ownership remains uncertain, report ambiguity
  and stop. Keep the interactive tmux session as the sole owner of questions
  and approvals.
- A session belongs to a host and a persisted harness identity, not merely to a
  display name or slot. Do not assign one session to multiple owners or forward
  a remote host's local control socket through SSH.
- Release updates install immutable, checksum-verified native builds and probe
  the candidate before activation. Remote workers must run the exact matching
  build/runtime. Never describe source-clone fast-forwarding as the update path.
- Creation may record a pending selection, but select the new row only after it
  appears in the prepared visible rows. A refresh completing alone is not proof
  that the UI has rendered that row.
- Optimistic external mutations keep their clobber guard until a fetched value
  catches up, the mutation fails, or the bounded deadline expires. A resolved
  request alone does not prove the read source has observed the write.
- Hold state is self-expiring and resource-scoped. Recheck the exact hold ID
  immediately before honoring it; an unknown or stale copy creates no freeze.

See [docs/architecture.md](docs/architecture.md) for the module map and
[docs/fleet.md](docs/fleet.md) for status and coordination policy.
