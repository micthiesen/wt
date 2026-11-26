## wt worktrees: status and testing ownership

In a wt-managed worktree you own the task's status, testing, and dev server.

- Assert each transition with `wt status <state> [-m note]` (bare `wt status`
  lists the states): `working` when work begins, `review` when review begins.
- Set the title on purpose: `wt title "<outcome>"` when work begins and again if
  the outcome changes (sentence case, about the PR title). A person's is final.
- Testing is yours: `needs-testing` means you owe verification, not the human.
- `needs-human` is only for an action outside the repository with no defensible
  default (interactive auth, a human-only call). Keep doing independent work.
- Decide reversible repository changes yourself. External messages, published
  artifacts, closed issues, and hosted writes need authorization.
- An escalation note is about 300 characters and needs no other context:

      ASK:       <the question, one sentence>
      RECOMMEND: <the default action, one sentence>
      WHY:       <the fact that decides it>

### Dev servers

- wt owns long-running dev servers: never start one with the repository's own
  dev command, even where its docs say to. Check `wt dev status`, start with
  `wt dev start --wait`, read output with `wt dev logs`. Short-lived preview
  or watch runners may run directly. If wt reports `[dev_server] is not
  configured`, follow the project's own server instructions.
- Exit 75 means every server slot is taken (`wt dev status --all` shows
  holders): queue with `--wait`. Never stop or restart another worktree's
  server; in a production emergency, notify the manager once and stay queued.
- Exit 0 means launched, not ready: before environment-dependent tests, check
  `wt dev status` for an unhealthy or pre-rebase server (`wt dev reset` fixes).
- Before handoff, run `wt dev stop` unless you still plan to use the server.
  A PR or an idle moment is not disuse.
- Logging in at each worktree's own browser origin is routine setup; script
  the login when it keeps needing a human.
- A timeout without an assertion failure may be machine load: run `wt perf`
  and confirm the changed code can reach that test before blaming the diff.

### Shared checkout safety

The checkout is shared with other agents, the human, and hot-reloading servers.

- Test a hypothesis against a copy, never with `git stash`, `checkout`,
  `restore`, or `reset`.
- Announce any needed change to shared tree state; restore it before finishing.
- Put scratch files in the harness scratch area, never in the checkout.

### Waiting

- While driving something that resolves within about an hour (CI, a review
  bot, the merge queue), stay and block on `wt wait [--any] <condition>...`,
  as a background command where the harness has them. Where it caps foreground
  commands (Claude Code: 10 minutes), pass `--timeout 9m`; rerun on exit 124.
- What outlives the session (a deploy, a release, a time hours away, another
  worktree's merge) is `wt wake <condition>... -m "<what to do then>"`, then
  end the turn; "tell me when X" is `wt notify`. Never sleep or poll. A wake
  fires only while wt is running.
- Conditions (all must hold; `wt watch` lists them): a time (`3h`); `merged`,
  `production`, `green`, `settled`, `reviewed`, `evicted`; `pr:3806:merged`;
  `wt:<slug>:merged`; `release:production`; `check:<id>=<arg>`. All but a
  time take a delay (`release:production+24h`).

### What you still owe

`wt owed` lists what this worktree owes and what closes it. A row owing
nothing is finished; the note never holds owed work:

- A prerequisite before merging is `--blocked-on "<gate>"`, only when merging
  now would make things worse until it happens (a manual deploy or follow-up
  that leaves the status quo unchanged is a step). `--until <condition>`
  clears it once wt observes the prerequisite, `--then "<prompt>"` acts then;
  only a person's decision stays a bare gate (`wt status --unblock`).
- A check only a deployed environment can run is `--verify-in-staging` and/or
  `--verify-in-production` (both may be owed), set as soon as you know it. It
  never blocks merging; the row returns as `needs-testing` once the change is
  there. While a follow-up fix is unmerged, stay `ready --verify-in-<stage>`:
  a carried check never shows an earlier one passed. Write it as:

      <one line: what only the deployed environment can prove>
      <why a local run cannot, when that is not obvious>
      STEPS: 1. <exact step: literal values, expected output> 2. ...

- Any other step, never a check: `wt wake <condition> -m "<step>"`, with
  `--before-release` when it must precede the next production release.
- Each stage holds one check; a second check for the same stage (another PR
  of this worktree) is a wake step: `wt wake pr:<n>:<stage> -m "<steps>"`.
- A due check, and a wake delivered after the work merged, stay owed (and hold
  off cleanup) until closed, even when moot: do it on a deploy that contains
  the change, then `wt done <id|staging|production> -m "<evidence>"`.
- Put off a due check with `wt done <stage> --again <time>`, never a `wt wake`.
- `wt done <id> --again <condition>` puts off a due step, `wt watch cancel <id>`
  (or `--mine`) drops a wake, `wt status --no-verify-in-<stage>` drops a check.
- Every op is yours: do it when due, asking approval where the action needs
  it, never handing it to the human. Prefer automation that ships with the
  change (a checked-in migration the normal deploy applies); a step the merge
  and deploy flow already performs is not owed.

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
    REVERT:   <"safe", or "no:" + the shortest true reason>
    IF WRONG: <where it shows + the symptom>
    UNTESTED: <checks nobody ran, creating no obligation; omit when none>

- Write `OPS: none` only when nothing is owed; wt refuses `OPS` phase lines.
- A branch that will never land: close its PR, `wt status dropped -m "<why>"`,
  then `wt watch cancel --mine`.

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
- Invoke tools by name through PATH, even in scripts, so wt's shims apply.
