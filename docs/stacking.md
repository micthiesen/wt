# Stacked PRs

`wt` carries an opinionated small-stacked-PR workflow: **implement a feature holistically on one branch, then carve the validated diff into a stack of small draft PRs** that review independently. The stack is a derived view of finished work, not a way of working in tiny units up front.

The moving parts:

- **`wt size`** reports a diff's production LOC + file count (tests, snapshots, generated files, and lockfiles excluded) — the budget you split against.
- **`wt stack`** is the manifest engine: it materializes a stack (worktree + draft PR per slice), renders its status, and restacks after a parent merges with a squash-safe replay. Subcommand reference: [cli.md](cli.md#stacked-prs).
- **Bundled agent skills** are the day-to-day driver. `/split` decides the slice partition, authors the manifest and PR bodies, and hands the mechanical work to `wt stack`; `/restack` handles rebasing after merges, including conflict resolution. A `wt` reference skill teaches the agent the CLI. Install them with:

```sh
wt skills install --harness claude     # or codex / opencode
```

- **In the TUI**, stacked slices render as a tree under their section, and `R` on any slice runs the whole stack through fetch + reconcile + replay without model involvement (a conflict bails to `/restack`). An `[[automations]]` rule with `on = "stack.parent_merged"` / `run = "builtin:restack"` makes that fully automatic — see [automations.md](automations.md).

## Key properties

- **Squash-safe.** Each slice records a `baseSha` anchor, so a parent landing via squash-merge doesn't make its commits reappear as conflicts in the children.
- **Conflicts are never auto-resolved.** A replay that hits one exits with the failing branch named and a `backup/restack-*` ref pointing at the pre-replay state; resolution belongs to a human or the `/restack` skill.
- **Manifest-driven, no heuristics.** The stack graph is exactly what the manifest says (slices, bases, PRs); `wt stack status` shows drift against live PR state, and `reconcile` folds merges back into the manifest.
- **Stack-on-stack works.** A slice's base can be another stack's tip; landing across the boundary reconciles automatically.

The full design rationale, locked decisions, and session history live in [stacking-workflow.md](stacking-workflow.md).
