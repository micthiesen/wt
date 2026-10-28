## wt worktrees: status and testing ownership

In a wt-managed worktree you own the task's status, its testing, and its dev
server.

- Assert each transition with `wt status <state> [-m note]` (bare `wt status`
  lists the states): `working` when work begins, `review` when review begins.
- Titles are set only on purpose. When work begins, if `wt title` shows an
  `issue` or `fallback` title, replace it once with the outcome, about the PR
  title: `wt title "<outcome>"`. A person's title is final.
- Testing is yours. `needs-testing` means you still owe verification, never
  the human.
- `needs-human` is only for an action outside the repository with no
  defensible default, such as interactive auth or a human-only judgment. Keep
  doing independent work meanwhile.
- Decide reversible repository changes yourself. External messages, published
  artifacts, closed issues, and hosted writes need authorization.
- An escalation note is about 300 characters and answerable without other
  context:

      ASK:       <the question, one sentence>
      RECOMMEND: <the default action, one sentence>
      WHY:       <the fact that decides it>

### Dev servers

- wt owns long-running dev servers: never start one with the repository's own
  dev command, even where its docs say to. Check `wt dev status`, start with
  `wt dev start --wait`, read output with `wt dev logs`. Short-lived preview
  or watch runners may run directly. If wt reports `[dev_server] is not
  configured`, follow the project's own server instructions.
- Exit 75 means every server slot is taken: queue with `--wait`
  (`wt dev status --all` shows holders). Never stop or restart another
  worktree's server; in a production emergency, notify the manager once and
  stay queued.
- Exit 0 means launched, not ready. Before environment-dependent tests, check
  `wt dev status` for an unhealthy or pre-rebase server; `wt dev reset`
  rebuilds it.
- Before handoff, run `wt dev stop` unless you still plan to use the server.
  A PR or an idle moment is not disuse.
- Logging in at each worktree's own browser origin is routine setup; script
  the login when it keeps needing a human.
- A timeout without an assertion failure may be machine load: run `wt perf`
  and confirm the changed code can reach that test before blaming the diff.

### Shared checkout safety

The checkout is shared with subagents, reviewers, other sessions, the human,
and hot-reloading servers.

- Test a hypothesis against a copy, never with `git stash`, `checkout`,
  `restore`, or `reset`.
- Announce any necessary change to shared tree state and restore it before
  finishing.
- Put scratch files in the harness scratch area, never in the checkout.

### Completion status

Never end a session without a status. Finished work is
`wt status ready --risk low|medium|high -m <note>`, with risk as your
confidence after testing, not the change's size:

- `low`: verified in a real environment, or pure logic covered by tests that
  fail against the old code.
- `medium`: correct by construction and unit-tested but not run in a real
  environment, or broad but plainly reverted.
- `high`: material behavior unverified and rollback is not a plain revert.

Amend it with `wt status --risk <r>` as evidence changes. The human merges
unless they hand you the branch; `ready` is where you stop. Arm auto-merge only
with `wt merge`, never `gh pr merge --auto`.

The ready note is about 400 characters of fragments:

    <one line: what changes, in user terms>
    OPS:      <migrations / redeploys / config, or "none">
    REVERT:   <"safe", or "no:" + the shortest true reason>
    IF WRONG: <where it shows + the symptom>
    UNTESTED: <checks nobody ran, creating no obligation; omit when none>

- `--blocked-on "<gate>"` only when merging now would make things worse until
  an external prerequisite happens, never only in the note. A manual deploy or
  follow-up that leaves the status quo unchanged is not a gate; it goes in
  `OPS:`. `wt status --unblock` clears a satisfied gate.
- `--verify-after-merge` as soon as you know a specific check can only run
  after deployment. It does not block merging; after merge the row returns as
  `needs-testing`. Write it as:

      <one line: what only the deployed environment can prove>
      <why a local run cannot, when that is not obvious>
      STEPS: 1. <exact step: literal values, expected output> 2. ...

  Once the deployment contains the change, run the steps, then
  `wt status verified -m "<what was checked and where>"`. While the follow-up
  fix for a carried obligation is unmerged, keep that change
  `ready --verify-after-merge`; carrying an obligation never shows an earlier
  check passed.
- A branch that will never land: close its PR, then
  `wt status dropped -m "<why>"`.

### Fleet coordination

- Make normal decisions yourself. `wt manager send "..."` is only for a
  concrete cross-owner decision, a shared-resource conflict, or releasing a
  slot another owner needs: no progress, acknowledgments, papercut forwards,
  sender prefixes, or harness peer messaging.
- Honor a hold only when `wt hold check <id>` returns `active: true`; messages
  never create or renew one. A hold pauses only the operations it names, and
  only during the actual mutation; continue unrelated work.
- Broken tooling never blocks its own repair: record the exact owed
  verification, run it once repaired, and continue independent work.
- Record first-hand cross-branch facts with
  `wt edge <from> <before|conflicts|enables> <to> [-m why]` (`--blocks` for a
  hard dependency). Edges expire when either branch moves; reassert them after
  a material change. A missing edge is not evidence of safety.
- Invoke tools by name through PATH, including in scripts, so wt's shims
  apply.
