# Rust implementation

The user authorized a complete rewrite on this branch. Keep work on this branch;
do not merge or promote it. The TypeScript behavior baseline is commit `d9cd2f4`
and is not included in this tree. Do not add it as a runtime dependency.

Use the parent instructions for behavioral contracts. Rust uses explicit
ownership, typed errors, Tokio tasks, and scoped resources.

- Keep domain transforms pure. Pass configuration, clocks, repository identity,
  and external adapters explicitly; never read global configuration at import or
  static initialization time.
- UI input and rendering must not do disk I/O, database work, process spawning,
  network requests, directory walks, or unbounded parsing. Render visible data
  from prepared snapshots. Avoid continuously drawing an idle screen.
- Bound queues and concurrent work. Coalesce replaceable snapshots and duplicate
  refresh requests, but never silently discard accepted commands or durable
  writes. Obsolete background results cannot overwrite newer state.
- Every long-running task and process has an owner and cancellation/shutdown
  behavior. Dropping a future alone is not proof that its child exited. Preserve
  locks until required cleanup and durable writes have completed.
- Preserve existing configuration, JSON protocols, database contents, and session
  identities. Retain unknown durable fields on read/modify/write. Separate
  disposable cache loss from durable-state failure.
- Use typed expected errors; add context at process/application boundaries.
  Do not hide errors with default values that imply healthy or absent state.
- Keep public APIs narrow, modules focused, and workspace dependencies centralized.
  Do not introduce abstractions without a concrete caller or test seam.
- Test changes using isolated directories, databases, sockets, and tmux servers.
  Never point destructive fixtures at a user's live repository or state.
- Run scoped tests during parallel work. The parent owns workspace-wide formatting,
  integration checks, dependency policy, commits, and release operations.

The migration inventory lives in `docs/rust-rewrite-inventory.md`; the execution
record is `docs/rust-rewrite.md`. Use their current evidence and open cutover
gates when assessing completion; a package-scoped test is not a workspace gate.
