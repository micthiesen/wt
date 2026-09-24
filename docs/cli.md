# CLI reference

`wt` with no arguments launches the TUI (when stdout is a TTY; piped output falls back to `wt ls`). Everything below is the one-shot subcommand surface. `wt <cmd> --help` prints per-command usage.

Environment variables: `WT_CONFIG` points at an explicit config file; `XDG_CONFIG_HOME` relocates the default lookup (see [configuration.md](configuration.md)). Both are forwarded into the `wt events` launchd daemon so it loads the same config.

### `wt init [directory]`

Create `.wt.toml` at the containing Git repository root. This command does not
require an existing valid wt config. It detects the trunk branch, creates
defaults for a sibling worktree root, and assigns path-derived cache and tmux
namespaces. It refuses to overwrite an existing file.

### `wt state migrate [--from <legacy-cache-dir>] [--keep-legacy]`

One-time upgrade for repositories that used the former shared `state.json` and
`archive.json`. Imports only live records attributable to the current
repository into `~/.local/state/wt/wt.sqlite`, backs up both legacy files, and
then removes only the imported records. Ambiguous removed history and bare
branch watermarks stay in the backup. Reruns are safe; current SQLite values
win. `--keep-legacy` performs a copy-only pass.

Also repairs a board split by an earlier build that derived repository identity
from the caller's working directory. It adopts rows filed under a per-worktree
namespace or written into a second state database, merging them FIELD-wise so a
tracker id recorded in one namespace and a work status asserted in the other
both survive; and it carries the runtime files that do not regenerate — the
`automations.json` once-only fire ledger, the `harness.json` primary-harness
pick, and the harness session-name registries — into this repository's cache
root. Those sources are read only: nothing is written to them and nothing is
deleted.

### `wt remote [<command> ...]`

With no arguments, allocate an SSH terminal and enter the `[remote]` host's
interactive `wt`. With arguments, forward the exact argv through a shell-safe
encoded transport to that installation—for example `wt remote ls --json` or
`wt remote new eng-123 --no-install`. Requires the optional `[remote]` config
and an independently configured `wt` on that host. Controller inventory uses
the private, versioned `_snapshot` endpoint; `ls --json` remains the public CLI
shape and is not the worker protocol.

## Worktree lifecycle

### `wt ls`

List all non-main worktrees (slug, stage when `[deploy.sst]` is configured, PR, status). Worktrees destroyed in the last 48h stay visible — a dim `recently merged:` footer under the table, and an empty list says why it's empty (`No active worktrees (2 archived today: x, y).`) — so "everything landed" never reads identically to "nothing exists". Derived from the existing removed-worktrees history; the TUI's `h` view keeps the full 14-day record.

- `--json` — machine-readable array (slug, branch, path, stage, section, base, status, `dev`, dirty, issue_id, issue_url, work_state, …). `dev` is the same `DevServerStatus` snapshot used by local rows and lets a controller render a worker's running, starting, queued, restarting, crashed, and stale-after-rebase states without a second SSH query. `section` is the manual TUI section the human placed the row in (`null` = inbox; inferred stack groupings never appear here — they're derived, not stored). `base` is the effective merge target (recorded fork base for stacked worktrees, else `[branch] base`) — never null. Recently-removed rows are appended with `pr`, `pr_url`, `title`, `archived_at`, and the work status the row held when its checkout went away (`work_state`, `verify_after_merge`, `verification_owed`). Those three are flat, matching `wt status --all --json` rather than `wt fleet --json`'s nested `work`, because all three commands append this same entry. **`verification_owed: false` with `work_state: null` means UNKNOWN, not clean** — the entry predates the field (added at state v10) or the row held no status; the per-slug record is reaped with the worktree, so for older entries the answer is genuinely gone. **Every row carries `kind`** — `"live"` for the worktrees that exist, `"merged"` (PR landed) or `"removed"` for the history — so filter on the value, never on the field's absence. It is the same field with the same values on `wt fleet --json` and `wt status --all --json`; a caller that skips it counts landed branches as fleet, which is how four removed rows nearly got filed into sections. Live rows were unlabelled until 2026-08-17, so the remote-section parser keeps a row whose `kind` is absent OR `"live"` (a presence test would empty the whole host section against a newer remote).
- Worker exception: `[instance] role = "worker"` always reports `section: null`.
  Fleet layout belongs to the controller, which stores remote placement under
  a host-qualified key and ignores any legacy section value from the worker.
- Push fields: `unpushed` counts commits `origin/<branch>` doesn't have — true unpushed work, not divergence from the base. `pushed` says whether `origin/<branch>` exists at all; when it's `false`, `unpushed` falls back to the ahead-of-base count. `ahead_of_base` is commits ahead of wt's effective merge base (the recorded stack parent or configured trunk), regardless of which git upstream the branch tracks. All three are `null` when git couldn't answer; never read `null` as 0. The trunk side of `ahead_of_base` is resolved in the MAIN CLONE when the checkout holds that commit — under `rift` a worktree's own `origin/<trunk>` is frozen at clone time, and counting against it charged the branch for every trunk commit that landed since (see [backends.md](backends.md#stale-remote-tracking-refs)).

### `wt new <id [title…]|url|branch|slug>`

Create a worktree from an issue id (optionally followed by pasted title words), a tracker URL, an existing branch name, or a bare slug. Runs the full setup: fetch, checkout (`git worktree add`, or a `rift` clone — see [backends.md](backends.md)), env-file and configured-glob copy, SST stage pin (only with `[deploy.sst]` configured), package install (detected from the lockfile, or `[lifecycle] install_command`).

Issue-id input resolves like this:

- `wt new ENG-1953 fix calendar rendering` — id + title words mints `yourname/eng-1953-fix-calendar-rendering`.
- `wt new ENG-1953` — bare id mints a fresh branch with a random readable suffix (`yourname/eng-1953-cozy-elephant`), so repeat entries just create more worktrees for the same task — that's the intended way to get a second one.
- `wt new --attach ENG-1953` — attach to that id's *existing* branch instead: one match checks out, several offer a picker (interactive shells only — scripted calls error and must pass the branch explicitly), none is an error.
- Multiword input without a leading id (`wt new fix the calendar`) slugifies wholesale to `yourname/fix-the-calendar` — issue-less worktrees are first-class.
- With `[issue_tracker] prefix` set (e.g. `"eng"`), a differently-prefixed id (`wt new GH-970 …`) is rejected with guidance: a GitHub issue attaches as the secondary id via `--gh`, never as the worktree's identity.

- `--slug <s>` — explicit slug when creating from an issue id (equivalent to inline title words; wins when both are given).
- `--gh <n>` — attach GitHub issue `#n` as the worktree's secondary id (see `wt issue`).
- `--attach` — attach to an existing branch for the id instead of minting a new one.
- `--base <ref>` — fork base to branch from (recorded; see `wt base`).
- `--any` — with `--attach`, match branches by any author, not just your `branch.prefix`.
- `--open` / `--no-open` — open in the configured editor after creation (default: open when interactive).
- `--no-install` — skip the package-install step. Ignored under the `rift` backend, which copies packages via its clone.

If the branch already has a worktree, prints its path instead of erroring. A dirty main clone never blocks creation: the `rift` backend's CoW clone copies the main clone's uncommitted changes, and the post-clone branch switch discards them in the copy (`--discard-changes` — the main clone itself is never touched). Any create failure surfaces its reason on the CLI and the TUI attention feed instead of a row silently vanishing.

Creation also sets `branch.<name>.gh-merge-base` to the branch's real merge target (its fork base; the trunk for plain worktrees, the parent for stacked ones). `gh pr create` consults that config **before** the repository's default branch, so a bare `gh pr create` opens against the right base even in repos whose GitHub default branch isn't the integration branch — without it, agents were opening PRs against the default (the harness's own context hints them there), burning CI and review passes on a 100-file diff of other people's code. `wt doctor` checks the config against the recorded base to catch worktrees created before this or left stale by a reparent.

### `wt rm [<slug>]`

Remove a worktree (with dirty/unpushed guards, optional SST stage destroy, optional branch delete). No slug ⇒ interactive picker.

"Unpushed" is measured against `origin/<branch>`, the same `unpushed` field [`wt ls --json`](#wt-ls) reports — never against `@{u}`, whose meaning depends on the branch's local tracking configuration. The guard is suppressed entirely for a merged/gone branch: a squash-merged worktree keeps its pre-squash commits locally but the work is landed, so it removes without a spurious `--force`. The TUI's `d` and `c` apply the same rule through `destroyHazard`.

- `--yes` / `-y` — skip confirmations.
- `--force` — remove despite uncommitted / unpushed work, or an outstanding post-merge verification.
- `--destroy-stage` / `--no-destroy-stage` — force the SST stage decision (default: prompt when your stage looks deployed).
- `--delete-branch` / `--keep-branch` — default deletes the branch.
- `--background` / `-b` — dispatch as a background job (watch with `wt logs <slug>`).

Removal also closes the browser tabs the worktree's agents opened, two ways. It deletes the worktree's `browser-control` session (`wt-<slug>`, the `BROWSER_CONTROL_SESSION` every harness session inherits — see [fleet.md](fleet.md#the-design-responses)), which closes that session's page while the relay still holds a live target for it; and it closes every browser tab parked on the worktree's dev port, by driving the running Chromium browsers over AppleScript. The second pass exists because the first silently stops closing anything once the debugger detaches from the tab (DevTools opened on it, "Cancel" on the debugging banner, a browser restart) — `session delete` still succeeds and still reports the id, but the tab lives on. Best-effort and silent: no `browser-control`, no relay, no browsing, or no Chromium browser open all mean nothing happens.

Tabs the human attached by hand are released rather than closed — except on the dev port, which is wt's own allocation and whose server is going away regardless, so everything on it goes. A detached tab that is NOT on the dev port (a staging URL, a PR page) has no handle left and survives; `browser-control` offers no way to reach a tab it isn't attached to, and wt won't close one by guessing at its URL.

And it reaps hand-started servers: any process holding a **listening TCP socket** whose cwd is inside the worktree (an agent's backgrounded `pnpm preview`, a stray vite) is SIGTERM'd — SIGKILL'd if it lingers — before the checkout is removed, with a `reaped …` line in the destroy log per kill. The listening-socket filter is the safety boundary: an editor or shell sitting in the directory holds no socket and is never touched. This is the cleanup half of the process contract — the managed half (`wt dev`) covers the one process meant to *outlive* an agent's session; everything else may run unmanaged precisely because destroy guarantees it can't outlive the worktree. Best-effort and silent when nothing is listening, which is the common case.

### `wt clean`

Remove every worktree that is merged or whose remote branch is gone. "Gone" is only auto-cleaned when a merged PR confirms the content actually landed; anything riskier is left for an explicit `wt rm`.

**Never forces, and has no `--force`.** A candidate that still owes a [post-merge verification](#--verify-after-merge-steps--the-check-that-can-only-run-once-it-ships) is listed under "Skipping" with the steps it owes and the command that discharges them — every candidate here has landed by construction, which is exactly when that check comes due, and sweeping the row deletes the obligation along with the checkout. A candidate holding uncommitted changes is listed under "Skipping" and kept, however thoroughly its branch landed: "merged" is a claim about the branch, and the working tree answers a different question — a merged worktree collects new edits the moment anyone reopens a session in it. Discarding work stays deliberate and one worktree at a time (`wt rm <slug> --force`). This guard lives here rather than in the backend because no backend supplies it: `rift remove` trashes a dirty checkout, and a rift worktree is an independent clone, so its objects, branch and reflog go with the directory (see [backends.md](backends.md)).

- `--yes` / `-y` — skip confirmation (required non-interactively).
- `--destroy-stage` / `--no-destroy-stage` — apply to all candidates (default: per-worktree, destroy iff its stage is live).
- `--foreground` — run removals synchronously (background dispatch is the default here, unlike `rm`).

### `wt merge [<slug>]` / `wt merge --cancel [<slug>]`

Arm GitHub's **"merge when ready"** on the worktree's PR — the same action as the button on the PR page, and the same dispatch the TUI's `m` picker row uses. Never merges on the spot: GitHub holds the PR until its requirements are met and merges it then. `--cancel` disarms.

**This exists because `gh pr merge --auto` is not a substitute, and fails in a way that reads as something else entirely.** Two different GitHub features wear the "merge when ready" label and they are gated on different settings: a base branch with a **merge queue** is enqueued (`enqueuePullRequest`, which needs nothing enabled at the repo level), and everything else arms **classic auto-merge** (`enablePullRequestAutoMerge`, which requires the repo's "Allow auto-merge"). gh only ever does the second. So on a repo that queues on `staging` with `allow_auto_merge: false`, the browser button works and the CLI returns `Auto merge is not allowed for this repository` — an error naming a repo setting that has nothing to do with the failure. An agent reads that as a permission it needs a human for, stops, and hands back a branch whose only remaining step was the button. `wt merge` picks the arm mutation off the PR's `baseRefName`. `--cancel` inspects the PR's actual state: a queue-base PR may have classic auto-merge armed but no queue entry. It dequeues only a real queue entry; `--disable-auto` alone would leave that entry queued to merge.

The merge METHOD is likewise not the caller's to choose on a queue base — the queue's own configuration sets it, so there is no `--squash` here.

Exit codes: `0` armed, `2` usage, `75` **temporary** — a required check has not reported for this commit yet, which is a clock rather than a verdict, so the caller should wait and re-run rather than escalate. `1` for everything else. `<slug>` defaults to `$WT_AGENT`, then to the worktree containing the current directory (longest matching path, since one slug is routinely a strict prefix of another).

### `wt doctor [<slug>]`

Health report: working tree, sync vs trunk, node_modules, locks, `gh-merge-base` branch config (must match the recorded fork base / trunk, or a bare `gh pr create` targets the repo default branch — see `wt new`), merged status, PR/CI. One worktree (or the one containing cwd), or all. Also banners machine-level issues: a main clone off its trunk branch, pending agent-skill updates (`wt skills`), Claude/Codex message-transport degradation, and **`wt` not being reachable on `PATH`** — a shell alias satisfies interactive use but doesn't exist inside a script file, so anything that scripts wt (an agent looping over worktrees) dies partway with `wt: command not found` and leaves the fleet half-updated. The check resolves `PATH` itself rather than shelling out, since this process's own shell may carry the alias and answer misleadingly; it also warns when a `wt` on `PATH` resolves to a *different* clone, which is worse than none.

- `--all` / `-a` — force the full summary table.
- `--json` — machine-readable.

Two checks are conditional rather than universal, because a check that can only ever warn is noise on the first command a new user runs:

- **SST stage pin + deploy state** (and the summary table's `stage` column) appear only when `[deploy.sst]` is configured. Without the integration there is no stage to pin, so the check would warn forever on every row.
- **node_modules** detects the package manager from the checkout's lockfile — the same detection `[lifecycle] install_command` defaults to — so the advice it prints (`run \`pnpm install\``, `run \`bun install\``, …) is the command wt would actually run. A checkout with no `package.json` reports the check as inapplicable instead of missing. Only pnpm gets two further probes, because it has the one layout where `node_modules` can exist and still be unusable. The first is existence of the store directory (`node_modules/.pnpm`). The second reads the top level: under `nodeLinker: isolated` every package entry is a symlink into that store, so a real directory there is something pnpm did not write and will not remove — installs prune the store against the lockfile and leave the top level alone. `pnpm install` then prints `Already up to date` over a tree whose resolution is frozen at whenever those directories were written. It warns only for the ones that actually resolve to a version the store lacks (naming up to 8, remedy `rm -rf node_modules && pnpm install`, since a plain install cannot clear them), and otherwise reports the count as info: still wrong, but not yet biting. Measured on a live checkout: 124 of 969 such directories, one of them resolving `@supabase/auth-js` at 2.72.0 against a lockfile holding 2.85.0. The failure this exists for is LOCAL-only and therefore unfalsifiable from either side — CI installs into an empty tree and is correct, the checkout is stale, and the disagreement carries nothing that says which is wrong. It is silent in both directions: a typecheck can fail on a member the real version has, or pass on one it doesn't. Any other linker (`hoisted`, and anything added later) makes real directories correct, so the check returns no opinion rather than a clean one, as does an unreadable `.modules.yaml`.

### `wt open [<slug-or-query>]`

Open a worktree in your editor (`[editor] command`; the default is Zed, with focus-if-already-open). Exact slug or case-insensitive substring; no query ⇒ interactive picker.

## Inspection & maintenance

A failing command prints the failure's message chain (`operation: cause`) and exits non-zero; `WT_DEBUG=1` appends the stack, which an unexpected (untagged) crash always prints.

### `wt stages`

List SST stages in the configured state bucket and flag orphans (no matching live worktree). Requires `[deploy.sst]`.

- `--clean` — destroy orphaned stages (`sst remove` per stage, in the main clone).
- `--yes` / `-y` — skip the destroy confirmation.
- `--json` — machine-readable `{live, orphaned}`.

### `wt dev <start|stop|status|logs> [<slug>]`

Manage the worktree's `[dev_server]` (see [configuration.md](configuration.md#dev_server--optional-per-worktree-dev-server)). `start` is also restart; `stop` keeps the slug's port reserved; `logs` prints the supervisor pane's recent output. The slug defaults to the worktree containing the current directory.

`stop` also closes the browser tabs that were on the server, matched by the dev port (not by session name, so the login-script sessions actually holding the app open are covered) — both the `browser-control` sessions sitting on that port and, over AppleScript, the browser's own tabs on it. Stopping the server strands those tabs on a refused port, so they go with it. The worktree's other browser sessions are deliberately untouched: an agent's reference tabs are not the dev server's. And it runs `[dev_server] stop_command` if the project set one, which is the only thing that releases what the dev command created *outside* its own process tree (docker containers above all).

If `stop_command` fails, `stop` exits 1 instead of claiming the entire environment stopped. `reset` also exits 1 without running the destructive `reset_command`. Both show the stop-hook errors: the wt supervisor has stopped, but external resources may be partly removed or still running. An exited database alone does not prove the rest of a container stack is gone; inspect the reported teardown failure before retrying reset.

Flags:

- `start --wait [--timeout <secs>]` — when `[dev_server] max_concurrent` is set and the fleet is full, queue until a slot opens instead of refusing. Default timeout 1800s; on expiry it exits `75` like a plain refusal. While queued the slug shows in `wt dev status --all` and on its own board row, so a waiting agent doesn't read as a stalled one.
- `status --all` — the fleet view: slots in use against the cap, every dev server and whether it's up or crashed, the queue with ages and tiers, and the machine's **1-minute load against its core count**. Works from anywhere; it needs no subject worktree. The load line is there because slots ration *stacks*, not load: a test run, a build or a type-check is neither queued nor counted, so every slot can read healthy on a box that is far past capacity (observed: 3 of 4 slots in use at load 84.9 on 12 cores). Past 2× cores it says so, and points at `wt perf` — a test timing out on a saturated box is measuring the box.
- `queue` — print the wait queue. `queue <slug> --first` moves a waiter ahead of every ordinary one; `queue <slug> --normal` gives its place back. See below.
- `status --json` — machine-readable form of either view.

**`start` JOINS a launch already in flight instead of restarting it.** `start` is also `restart`, and killing a *running* server to bring it back on new config is the point of that — but a supervisor still in its startup phase is a different thing, and killing one is destructive in a way nothing downstream can see. The sequence that found it was wt's own advice: `wt dev reset` ends by launching and returns 0, and the banner used to say "run `wt dev start --wait` to wait for it", which killed the launch it had just made, mid-boot, against an environment whose state the reset had just dropped. Two worktrees hit it independently. The crash has two faces (a database connection error while the baseline migration applies; "did not return the local URL and service-role key" with every service stopped), and the residue is worse than the crash: a half-initialised volume survives, so the next start finds an existing environment, takes its reuse path, and never re-provisions what only a fresh initialisation creates — a storage catalog reading 0 of 22 buckets while the migration ledger reports fully migrated. That also explains the intermittency: only a kill *during* initialisation leaves the broken volume. The banner now names the `--wait` on the command you actually ran.

**Exit `75` from `start` means the concurrency cap is full, not that anything is broken.** It's sysexits' `EX_TEMPFAIL`; retry later, or use `--wait` and let wt do the retrying. The refusal names who holds the slots, and names crashed holders specifically — a parked supervisor still holds its slot (its containers are still up) and is the cheapest one to reclaim. Semantics: [configuration.md](configuration.md#max_concurrent--the-load-governor).

**`wt dev queue <slug> --first` is how one worktree goes first, and it exists because asking nicely loses a race it cannot win.** Getting an urgent worktree (a coded fix for a live data-loss bug) to the front of a full queue used to take four messages: ask a holder to release, ask the queue leader to step aside, watch the freed slot go to that leader anyway because a promotion is instant and a message to an agent is not, then ask for it back. Three agents cooperated correctly and the ordering still came out wrong, because nothing was written down when the slot opened.

Promotion edits the waiter's own queue entry, so it needs nothing from the promoted agent — its next poll re-reads the queue and finds itself at the front. There is no window to lose. It is a **tier, not an index**: `--first` sorts ahead of every ordinary waiter with arrival order preserved inside each tier, so nothing has to be renumbered when a waiter joins, leaves, or is pruned for having died. And it inherits the waiting room's self-expiry — the priority lives in the waiter's file and is gone with that pid, so a promotion covers the current wait and no more. Re-queueing later starts ordinary again, deliberately: if it still matters, whoever has the fleet context says so again.

Two rules follow from where the knowledge lives. Only an already-queued worktree can be moved (`wt dev start --wait` first), because a priority with no waiter attached has nothing to expire it. And **a worktree cannot promote itself** — relative urgency across a fleet is not knowable from inside one of them, every task looks urgent to the agent doing it, and a tier anyone can claim is a tier everyone claims; the refusal points at `wt manager send`. A human's shell carries no `WT_AGENT` and is never caught by this.

A plain `wt dev start` normally takes any free slot without consulting the queue — an ordinary waiter loses at most one poll interval, and being told "full" while a slot is visibly free would be the worse lie. It does **not** barge past a promoted waiter: that is a deliberate decision rather than a default, and the refusal says so specifically ("a slot is free but held for X") rather than reporting a capacity problem that would send the reader hunting the wrong worktree.

`logs` falls back to a saved copy of the scrollback when the session is gone: a parked supervisor's pane is captured to disk before anything reclaims its slot, so the crash report outlives the pane that held it.

### `wt logs [<slug>]`

Tail a destroy log (`tail -F`). No slug ⇒ the most recently modified log.

### `wt perf [--json]`

One-shot perf snapshot framed as **wt-downstream vs the rest of the machine** — the headless form of the TUI's [`P` overlay](tui.md#perf-overlay-p): verdict numbers, per-category and per-worktree-session breakdowns, the heaviest processes on both sides, and any leaked headless wt instances. The default output is the same plain-text report the overlay's `i` key sends, written for handing to an agent ("is this load reasonable, and if not, whose is it?"); `--json` emits the raw `PerfSnapshot` instead. Same accuracy caveats as the overlay: `%CPU` is `ps`'s lifetime decaying average, not a profile.

Unlike the overlay (where the TUI process itself anchors the tree), the CLI also roots at any live wt instance it finds in the process table, so the running TUI counts as "us" rather than showing up as an outsider.

### `wt base <slug>` / `wt base set <slug> <ref>` / `wt base clear <slug>`

Show / record / forget a worktree's fork base — the branch it's based on when that isn't trunk. This record is the stack primitive (see [stacked-prs.md](stacked-prs.md)): the TUI's base row, stack grouping, sync counts, diff, and AI summary all resolve against it, and `wt restack` replays onto it.

### `wt status [<slug>] [<state>] [-m <note>] [--risk <r>] [--blocked-on <gate>] [--verify-after-merge <steps>]`

Show or assert a worktree's **work status** — the agent-declared lifecycle state (`todo`, `working`, `review`, `needs-testing`, `needs-human`, `ready`, `verified`, `dropped`), rendered as the list pane's leftmost colored dot and used for the section-internal auto-sort (`[ui] sort`). The primary caller is a coding agent inside the worktree (cwd resolves the target; a slug/branch arg overrides), and the output deliberately teaches: every transition prints a short guidance footer with the expected next step, bare `wt status` prints the vocabulary (inside a worktree *and* outside one — outside, it adds a "pass a slug" hint instead of erroring), and errors restate the rules. `WT_NO_HINTS=1` silences the footers.

The rules that make statuses trustworthy are enforced here (the TUI's `u` picker is deliberately lenient for the human):

- `needs-human` requires `-m` naming exactly what's needed AND what was already tried ("blocked on X; tried Y, Z") — it's the only state that means "the human must act".
- `dropped` requires `-m` saying why the branch will never land (superseded, duplicate tracker id, deliberately not pursued) and takes no `--risk` — risk is a merge concept and nothing is being merged. It is the OTHER terminal state: where `ready` asks for merge attention, `dropped` asks to stop being looked at, so the row sinks below `todo` in the sort (a dim circle-slash dot) instead of wearing a fake `ready` whose note says "nothing to merge" — which would defeat the risk field's whole triage-without-reading-notes purpose. Merged/gone stays derived and is not this: `dropped` is the assertion the machine cannot make (a closed PR isn't proof — branches are dropped pre-PR, and closed PRs reopen).
- `verified` requires `-m` saying what was checked and where, and takes no `--risk` — the merge already happened. It means *merged AND confirmed in the environment the change actually deploys to*, and it is the only honest discharge of a `--verify-after-merge` obligation. A bare `verified` would be the same unevidenced claim the obligation exists to replace.
- `ready` requires `--risk low|medium|high`, and medium/high additionally require `-m`. **Risk means the asserter's residual uncertainty after testing, not the change's blast radius** — blast radius is already on the PR; confidence is the one thing only whoever did the work knows, and it's what the human sorts by when merging without reading the code. `low` = verified in a real environment, or pure logic with tests that fail against the old code; `medium` = correct by construction and unit-tested but never exercised for real, or plainly revertable but broad; `high` = something material is unverified AND backing it out isn't a plain revert. So a migration verified end to end on dev is genuinely `low`, and an untested one-line frontend change is not. Read as blast radius the field collapses (every migration `medium`, every tweak `low`) and carries no signal at all.

**`--blocked-on "<gate>"` decorates a `ready` that must not be merged yet.** It is not a seventh state, because "the work is finished" and "it cannot land yet" are two facts and a state can only carry one of them — which is exactly how this went wrong. A worktree whose migration would have broken guest join on every shipped mobile build asserted `ready --risk low` while writing `BLOCKED ON A MOBILE RELEASE` into its own note, one field to the left; the fleet manager read the state, put the branch in a merge order, and told the human it was mergeable, twice. Prose beside a field loses to the field, so the gate is a field and the *rendering* changes with it: the row drops out of the merge band in the sort, its dot becomes a warn-colored circle-slash instead of ready's green, the details banner reads `blocked · ready` with the gate on its own full-width line, `wt fleet` shows `blocked/ready`, and a `status.ready` automation does not fire (a "this is yours to merge" banner is the same misread, amplified).

**The state supplies the verb.** `ready` + gate means *do not merge yet*; `todo` + gate means *deliberately not started yet*, which is the more common one in practice. A fleet coordinator holding fourteen worktrees on an unlanded credentials file had nowhere to say so, so the policy lived in section names it had invented ("Held: prompt written, deliberately not started") plus its own memory of which gate applied where and what would clear it — none of which survives a compaction. A gated `todo` reads as held rather than merely untouched, and sorts below the todos someone could actually pick up. No other state takes a gate: the in-flight states describe work in motion, where "blocked" already has a word (`needs-human`), and `dropped` waits on nothing.

For the `ready` form, scope is exactly *do not merge yet*, and the test is whether **merging makes something worse than not merging**. A revocation landing before the mobile build that tolerates it breaks every shipped client the moment it merges: gate. A migration someone applies by hand, or functions to redeploy: not a gate — merging causes nothing until someone follows through, and forgetting leaves the status quo intact.

That second case is the boundary that will erode the field if it is let in, because the two feel alike. A policy tightening whose migration is manual is **safe to merge and dangerous to forget**: unapplied, the bucket stays exactly as open as it is today, and the only new harm is a PR that reads as shipped. That hazard is real and it is what the note's `OPS:` line is for — read at merge time, and not a reason to hold the merge. "Merging has a consequence somebody must follow through on" is not "merging is unsafe"; admit the first and the field comes to mean "read the note", which is what it replaced. The flag is refused on any state but `ready` or `todo` (still working on it is `working`; blocked on the human to make progress at all is `needs-human`).

Nothing expires a gate — wt cannot observe a release shipping — so it clears when someone says so: `wt status --unblock` amends in place, keeping the state, risk, note and timestamp. Because the timestamp is kept, the `status.ready` fire suppressed while the gate stood is still unconsumed and lands when it clears. A gate left set after it cleared parks a mergeable branch, which is the safe direction to be wrong in; the record's age is what flags one worth re-checking. A fresh assertion (including any pick in the `u` picker) replaces the whole record, gate included.

**The `ready` note has a shape and a budget** (both taught in the command's own output): ~400 characters of fragments — one line of what changes in user terms, then `OPS:` (migrations/redeploys, or "none"), `REVERT:` ("safe", or "no:" and the shortest true reason), `IF WRONG:` (where it shows + the symptom), and `UNTESTED:` omitted entirely when nothing is. Detail belongs in the PR body; the note may point at it. The four fields are the questions someone merging unread code actually has — `REVERT` is the one nobody volunteers unprompted and the one that decides whether a bad merge costs thirty seconds or an afternoon, and `UNTESTED` is the honest twin of the risk level (omitting the line is what makes its presence a signal). The budget is load-bearing rather than style advice: each agent judges its own note in isolation and never sees the wall the human reads at once, so "concise" loses to "thorough" every time it's left to judgment. A `ready` note more than 25% over budget gets a hint naming its length; nothing is ever refused for being long.

**`-m` replaces the note, and the replaced text is echoed back.** Notes are durable, cross-session, human-facing state, so a one-clause `-m` on a routine transition used to silently destroy whatever was there (observed: a redeploy list, a schema-version warning, and which functions had been verified, all lost to `review -m "addressing review findings"` — with nothing in the output to suggest it). Now any note that stops being the record's note is printed back under `previous note (now gone)`, which puts it in the scrollback of the process that caused the loss at the moment it happens — recovery is a copy-paste, and after a compaction that matters, because the agent can't retype what it no longer remembers. `--append` adds to the existing note instead of replacing it (works with `-m` and `--note-only`). A fresh assertion still starts a clean note when none is given — a `needs-human` note riding forward into `ready` would be worse than losing it — but that drop is now reported the same way.

States accept unique prefixes plus `nh`/`nt` aliases. `--clear` drops the record, `--all [--json]` prints the fleet overview (the manager session's eyes) — the JSON form carries each row's manual `section` (the human's grouping intent) and appends the same recently-removed rows as `wt ls --json` (`kind: "merged"|"removed"`, `pr`, `archived_at`, `work_state`, `verify_after_merge`, `verification_owed`), so an all-merged fleet is distinguishable from an empty one. **Branch on `kind` before reading anything else**: live rows are `kind: "live"` and are the only ones carrying `state`/`risk`/`note`/`at`/`by`/`stale`, and in a fleet where most worktrees have landed the archived rows are the majority, so iterating and reading `.state` throws on the first one. Two in-place amendments edit an existing record without faking a fresh assertion (both keep the state, `at` timestamp and sha, and both error when no status is asserted): `--note-only "..."` rewrites the note alone, and `--risk <r>` **with no state** re-judges the risk alone (optionally with `-m` to replace the note too). Re-judging is expected — risk is a confidence call that moves as testing lands — and having to restate a whole assertion to move it is what drives agents to append to notes instead of fixing them. Each record stamps the assert time and HEAD sha, so both the CLI and the details-pane `status` row can flag a status that predates newer commits. It also stamps **who asserted it** (`by`, from the `WT_AGENT` identity wt puts on every session it launches: a worktree slug, or `manager`; absent for the human's `u` picker and for a plain shell — and flat as `.by` on this surface, against `.work.by` on `wt fleet --json`). Statuses are routinely asserted on a worktree's behalf — the manager sharpens a `needs-human` note once it has triaged — and without the stamp the record can't say whether the claim comes from the worker that hit the blocker or from the coordinator that confirmed it. The CLI prints `via <who>` only when it wasn't the worktree's own agent, and [automations.md](automations.md#a-briefing-never-echoes-its-own-audience) covers the other thing it buys. Re-asserting an identical status (same state, note, risk, and HEAD) is a no-op that keeps the original timestamp — agents and hooks can assert freely without re-narrating (and re-toasting) the same news in every watching TUI. Statuses also ride `wt ls --json` (`work_state`/`work_note`/`work_risk`/`work_blocked_on`/`work_verify_after_merge`/`work_at`), which carries them across SSH for remote worktrees.

**The merge gate is on every status-carrying surface, and it overrides `state` on all of them.** `blocked_on` here (flat, like `.by`), `.work.blockedOn` on [`wt fleet --json`](#wt-fleet---json) (nested, like `.work.by`), `work_blocked_on` on `wt ls --json`. Non-null means **do not merge**, whatever `state` says — a consumer that reads `state == "ready"` alone repeats the failure the field exists for. `--unblock` is the only thing that clears it without re-asserting.

**So is the post-merge verification, and it reads the opposite way.** `verify_after_merge` here (flat), `.work.verifyAfterMerge` on `wt fleet --json` (nested), `work_verify_after_merge` on `wt ls --json`. Non-null means the branch **should** be merged; on a row that already has, it means the check has not happened and the worktree is deliberately being kept alive for it. `wt fleet`'s work column renders that row as `unverified/<state>` — and it resolves "landed" from the local merged/gone signals as well as the PR, because a squash merge the PR fetch missed would otherwise read as a plain `ready` on the exact row this is for.

#### `--verify-after-merge "<steps>"` — the check that can only run once it ships

Some claims can only be proved by the deployed environment: an OAuth consent screen against the real provider, a live third-party callback, an SDK with no local double. The work finishes, tests as far as local allows, goes `ready`, merges — and the actual verification never happens, because by then the row has sunk to the bottom of its section, the worktree is a `wt clean` candidate, and the context is gone. The workaround people invent is to remember not to delete the worktree, which works exactly as long as they remember.

The test for asserting it: **is there a claim in this branch that only the deployed environment can prove?** If so, name the exact STEPS in the field, not in the note. The agent that eventually runs them is not the one that wrote it, and may be reading it after a compaction.

**It has a shape, and no length budget** — an opening line that works as a headline on its own, optionally why a local run cannot prove the claim, then `STEPS: 1. … 2. …` numbered from 1:

```
Only a real Twilio callback can prove this. Local dev holds TEST credentials
that cannot reach the Video API (error 20008).
STEPS: 1. Confirm the deploy landed BEFORE believing any negative. 2. Create a
guest-only meeting and open its guest link. 3. Assert the ParticipantIdentity
is guest_<name>_<epochms>-<16 hex>.
```

Write the steps in full: this is the one field deliberately allowed to be verbose, because the literal strings *are* the payload (the exact identity format, the exact log line, the error code) and a tidier summary of them is a lossy rewrite of a spec. Nothing in wt ever summarizes it. The shape is instead what lets the surfaces stay readable **without touching the text**: the details pane collapses it to its opening lines until the branch lands (`V` expands, see [tui.md](tui.md)), while the attention feed, the `wt status` confirmation and the `UNVERIFIED — owed:` footers in `wt ls` / `wt fleet` / `wt claude` carry the first sentence and mark the cut with `...`. Every one of those recovers the full field from the record — `wt status <slug>`, `--json`, and the log file always print it whole. Which is also why the opening line has to stand alone, and the steps have to be numbered: an unnumbered wall cannot be counted, previewed, or resumed halfway.

**It is the opposite of `--blocked-on`, and confusing the two would destroy it.** A gate holds a row OUT of the merge band; this changes nothing before the merge. The row sorts and renders exactly as `ready`, because merging is not what it gates — merging is the *prerequisite*. If it ever sinks a mergeable row it has become a second `--blocked-on`, which is the failure it exists to avoid.

What it does instead is survive the merge, and that is the whole job:

- The row stops sinking to the landed rank and sorts as `needs-testing` — which is precisely what it now is (built, manual verification pending, and the **agent** owns that testing). No new mid-state was needed.
- Its list dot takes the marker slot back from the merge glyph. "Merged" reads as finished, and a finished-looking row is how the check stops happening.
- It becomes a **destroy hazard**: the `c` sweep and `wt clean` keep the worktree and say why, and `d` / `wt rm` refuse without `--force`. Nothing in the checkout is unrecoverable — the *obligation* is, and sweeping the row deletes the only record that anything was owed.
- Past `after_days` (default 2) the dot goes red, the banner reads `OVERDUE`, and a [`status.verification_overdue`](automations.md) automation can fire daily. A post-merge test nobody runs is worse than not tracking it at all, because the row reads as covered.

**Deploys lag merges**, and the guidance footer says so at every step: confirm the deploy carrying the change actually landed before believing a negative result. An instrument that cannot yet produce the positive is not evidence of the negative.

Unlike a gate it is dormant until the branch lands, so there is nothing to protect by restricting which state may carry it — record it the moment it is known. The two refusals are contradictions rather than policy: `verified` *is* the discharge, and a `dropped` branch has nothing deployed to check. It is also the one field a fresh assertion **carries** rather than drops: `note`, `risk` and `blockedOn` are claims made by an assertion, while this is a standing obligation about the branch, and a routine `wt status working` silently releasing the merged worktree back to the sweep is a failure with no output at all. `verified` and `dropped` end it; `--clear` drops the whole record. With no state argument, `--verify-after-merge "..."` amends in place (obligations are usually learned late — while writing the PR body, or when a reviewer names the thing only the real provider can prove).

**`--examined "<verdict>"` records that somebody with fleet context LOOKED and what they concluded**, stamped with the row's current HEAD. It is not a status: the row's own lifecycle state is untouched, because this is a claim by an observer rather than by the owner. It exists for the sweep case — a coordinator re-deriving the same verdict every few minutes. One ran the same two-call review query against the same two PRs on four consecutive passes and got the same empty answer each time, because the rows kept *looking* interesting (a review job reports failed while its review is still running) and nothing recorded that the question had already been asked and answered.

It is a **skip hint, never authority**. Absent, stale, or unrecognised all mean "look properly", so the failure direction is wasted work rather than a missed row. And it is write-once and self-expiring, which is the only reason it is safe to store at all.

**It is anchored on two SHAs — the row's HEAD and its base's — and `examined_current` requires both.** The base half is what keeps the safety property true. A pull request goes from behind to conflicted because the BASE moved, which leaves the row's own head untouched; anchoring on the row alone would keep a verdict valid across exactly the transition that voids it, and a sweep would skip the row that most needs attention. That inverts the failure direction from wasted work to a missed row, which is the one thing this field must never do. Caught before it bit, when a coordinator was about to stamp "behind only, no action" across 28 pull requests at the start of a merge batch. Invalidating on base movement costs re-examination while the trunk is moving and nothing while it is quiet — the right way round twice, since a settled fleet is when skipping pays and a moving trunk is when every mergeability verdict is suspect. The base tip is resolved in the **main clone** (`baseTipSha`), not in the worktree, and both the write and the read use that one helper. Under the `rift` backend a worktree is an independent clone whose `origin/<trunk>` is only as fresh as its own last fetch — measured at 10 distinct answers across 28 rows, one of them 9 merges behind — so anchoring on the worktree's view would pin the verdict to a SHA that trunk moving never changes, which is the missed-row failure all over again. A record written before the base anchor existed cannot prove the base held still, so it reads as not-current rather than current. `wt status --all --json` carries `examined` plus `examined_current` (the boolean a sweep keys its early-out on).

### `wt edge [<from> <kind> <to>]`

Merge edges: **pairwise, self-expiring** ordering hints between worktrees — the structured form of "merge A before B" that used to live only in manager prose ([fleet.md](fleet.md#the-design-responses) has the design rationale). `wt edge <from> before <to> [--blocks|--prefer] [-m why]` asserts (kinds: `before`, `enables` — orders the same, names a truth dependency; `conflicts` — same-files sequencing, direction irrelevant, no ordering effect; unique prefixes accepted). Bare `wt edge` lists (`--json` for machines, with computed `stale`), `wt edge rm <from> <to>` drops, and `wt edge prune` drops every edge whose endpoint is no longer a live worktree — that otherwise only happens at the startup reap, so edges pointing at branches that merged hours ago keep listing. `rm` resolves an endpoint through the removed-worktree history as well as live worktrees (a merged branch no longer resolves by name, which is exactly when you're cleaning up), and when the pair doesn't match it prints the edges touching either endpoint rather than a bare refusal — the two causes are direction (edges are ordered pairs) and a name that isn't the stored slug, and neither is visible otherwise. Endpoints accept slugs or branches; `by` records the asserting worktree (cwd) or `fleet`. Edges also ride each endpoint's row on [`wt fleet --json`](#wt-fleet) (`edges`, staleness computed) and are listed under the human table there, so merge planning reads them from the surface it already reads.

Every edge anchors both endpoints' HEAD SHAs at assert time and **expires when either branch moves** — stale edges grey in listings and stop steering the TUI sort until re-asserted (the CLI refuses an edge whose anchors can't be resolved: an edge that can't self-expire is worse than none). Edges with a merged/destroyed endpoint are dropped at reap. The TUI topologically orders rows within their section to honor fresh edges; `--blocks` vs `--prefer` (the default) tells the human which edges are safe to deliberately violate — nothing ever gates a merge.

### `wt section` / `wt section mv <slug>… <section>` / `wt section rename <old> <new>` / `wt section rm <section>`

Read and write the fleet's **sections** — the human's batching of worktrees ("To Merge", "On Hold", "Investigations"), rendered as the list pane's groups. Bare `wt section` (or `ls`) lists each section in display order with its rows, marking folded ones (a folded section hides its rows, which reads exactly like a row that moved); `--json` gives `[{name, folded, slugs}]` with the inbox last as `name: null`. `mv` moves a worktree's whole **stack** by default (a stack is one merge unit) and reports `moved N (stack)`; `--only` moves just the named ones. Splitting a stack across sections is legitimate rather than a mistake — finished parents awaiting verification and their unstarted children genuinely belong in different buckets — so wt never auto-reconciles a split, and a row already in the target section is left alone rather than re-placed at the bottom. `mv` takes the section as the LAST positional so several rows land in one call, creating the section if it's new and matching an existing one case-insensitively (`mv x "to merge"` finds "To Merge" rather than forking a near-duplicate); `-` as the section means the inbox. **A batch moves what it can and names what it could not** (exit 1 when anything was skipped, so a script still sees the shortfall; re-running is idempotent). The case that shapes it is not a typo but a race a fleet manager hits routinely: read `wt section`, build an `mv` from it, and one of those rows archives on merge in between. Bailing on the first bad name moved *nothing* while printing *one* slug, which read as "that one failed" and left the reader checking rows they believed had moved. `rename` onto an existing name **merges** into it, keeping fold state and relative order. `rm` drops the section; its rows fall back to the inbox and no worktree is touched. Stack groupings never appear here — those are derived from base records, so naming one would be writing down something wt recomputes.

Sections stay **asserted, never derived** (nothing in wt infers one), but they are no longer human-only: an agent that helped decide a merge batch can now record it instead of describing it and leaving the human to replay the conversation into the TUI by hand. Moves made from outside the TUI land on the **attention feed** — a grouping change the human didn't make is exactly the kind of thing that must not be discovered later, while their own `l`-picker moves stay on the firehose.

Sections are controller-owned. In `[instance] role = "worker"`, this command
refuses to run; remote rows are arranged from the controller TUI and the
worker's checkout/state is untouched.

### `wt fleet`

The [manager session](manager.md)'s single audit surface: one row per live worktree joining the **asserted** work status (state, note, risk, `at`, staleness vs HEAD) with observable **reality** — the active harness session's liveness (`alive`/`busy`/`last_activity`, using the same discovery behind `wt agent ls --json`) and the PR (number, title, draft, merge state, mergeability, CI rollup), all from the same single batched GraphQL round trip the TUI uses (never per-row `gh` calls). Each row also carries the human's manual TUI **section** — a second channel of asserted intent alongside the work status (a name like "Merge after Release" is a merge-ordering hint the manager should weigh; `null`/`—` = inbox, and inferred stack groupings never appear — those are derivable from base records and PRs). Rows sort ready-first, then needs-human (the TUI's urgency ranking), and the recently-removed rows ride along like on every fleet surface.

- `--json` — the contract. Live rows carry `section` plus nested `work`, `session`, and `pr` objects. **The `work` block is nested here and flat in `wt status --all --json`** (this surface joins three domains and has to namespace them; that one is status-only). So the asserter is `.work.by` here and `.by` there, and getting it backwards returns `null` rather than an error — indistinguishable from the `null` that legitimately means "unattributed", which reads as "the field isn't populated" instead of "I asked for the wrong path". Review state is **three separate numbers**, because collapsing them made the field lie: `unresolved_threads` is every open review thread (what GitHub's PR page shows), `unresolved_human_threads` excludes bot-opened ones, and `review_bot` is the bot's own rollup — in `checklist` mode the unticked-box count from its summary comment, which thread resolution does **not** affect. `review_bot.stale` is the half that decides what `state` is worth: it means the bot has not reviewed the CURRENT head, so `clean` there says "found nothing in an older commit", not "passed". The TUI already refuses to paint that green, and this surface used to omit the flag entirely, handing every JSON reader exactly the reading the badge withholds. It matters most to whoever acts on it rather than displays it: an agent working through a bot's follow-up reviews (`/babysit`) stops at the first stale clean, believing the commit it just pushed came back empty. Checklist mode only; `false` otherwise, which is what every reader already inferred from the absent field. On a repo where all review is done by a bot, the human count is permanently 0, so reporting only it reads as "nothing to chase" while the bot sits on unaddressed findings; when GitHub is unreachable (no `gh`, not authenticated, fetch failure) rows still emit with `pr: null` plus a `pr_note` saying why — so "no PR" (`pr` and `pr_note` both null) stays distinguishable from "couldn't ask". Removed rows are the same `kind: "merged"|"removed"` entries as `wt ls --json`, and live rows are `kind: "live"` — branch on the value before reading `work`, which only live rows have. **The stack and its ordering ride the row too**: `base` is the effective merge target (the recorded fork base, else `[branch] base`; never null, same derivation and value as `wt ls --json`) and `edges` are the merge edges touching that slug, the same objects `wt edge --json` prints plus `stale`. An edge rides **both** of its endpoint rows, so dedupe on `from`/`to`, and ignore stale ones for ordering as every other consumer does. Neither field existed here until 2026-08-31, which cost exactly what a missing field on a primary sense costs: a real four-deep stack carrying three `blocks` edges read as four independent branches with no constraints, and the manager reported to the human that no ordering existed — twice. `wt edge --help` had promised this surface read edges the whole time, and the omission was undetectable from the output, because absence of an edge is *defined* to mean "no known constraint".

Merge fields (`merge_state` from GitHub's `mergeStateStatus`, `mergeable`) are lowercased GitHub enums with one twist: GitHub computes mergeability **lazily**, so its `UNKNOWN` is reported as `"computing"` and wt never polls — re-run after a few seconds if you need the answer (the query itself is what triggers the computation). On terminal (merged/closed) PRs the merge fields are null rather than eternally "computing".

### `wt manager` / `wt manager send <text…>` / `wt manager report [--ok|--warn|--err] <text…>`

Attach the singleton [manager session](manager.md) (create on first use), or send a message to it. This is the fire-and-forget outbound channel for worktree agents and scripts, carrying both fleet-level questions and `papercut:` reports. `wt manager send` cold-starts the session detached when it is not running; the message lands as its next turn, and nothing comes back. Same session the TUI's `m` key enters. For Codex, both shell attachment and detached startup resume the manager-owned primary conversation; the main-clone `.` slot has separate ownership (see [migration behavior](manager.md)).

`wt manager report` is the reverse channel: it appends a short result line to a spool a running TUI watches and surfaces on the **attention feed** (with a toast). It's how [`M` palette](manager.md#the-command-palette-m) commands hand their outcome back without the human attaching; the level flag (default `info`) picks the line's color/loudness. Reports while no TUI runs aren't replayed later — it's a live-delivery channel, not a log (the daily log records whatever surfaced).

### `wt issue <slug>` / `--id <ID>` / `--no-id` / `--clear-id` / `--gh <n>` / `--clear-gh`

`wt issue [<slug>] --read` fetches the full primary tracker task with the configured
`[issue_tracker] read_command`. Omit the slug to resolve the worktree containing
cwd, including subdirectories. Uses the same attachment override/no-id semantics
as identity lookup. No attached tracker task skips explicitly (exit 0); no reader
configured exits 3. Failed readers preserve their output and exit code (child
exit 3 becomes 1, keeping the unconfigured result distinct), with an
incomplete-context warning; timeout exits 124. No provider-specific API or login
is built into wt. Read-only identity lookup remains offline.

Show or edit a worktree's issue links. The **primary** tracker id is normally parsed from the slug (`eng-1935-…` → `ENG-1935`); `--id COZ-2185` stores an override for a worktree whose slug carries none (or the wrong one), and `--clear-id` drops it back to the parse.

The override has **three** states, because two could not say everything. Absent means "use the slug", a value means "use this", and `--no-id` asserts **this worktree has no tracker issue** — stored as an empty override, which is why `resolveIssueId` distinguishes absent from empty and why the parse deliberately round-trips `""`. Only on a slug that carries an id does the difference show, and that is exactly the population that needs it: `--clear-id` on `coz-2101-connector-research` fell straight back to `COZ-2101`, so before `--no-id` there was no way to say the work is not that ticket. An asserted none renders nothing in the issue row, makes `{{issue_id}}` empty, and fails `requires = ["issue.tracker"]`, so the tracker automation stays put rather than moving somebody else's ticket. Reversible: `--clear-id` restores the derived answer. The TUI equivalent is `#` with an empty line. Readers resolve override-first, so the override is what the issue row links, what `{{issue_id}}` renders, and what `requires = ["issue.tracker"]` tests — the TUI equivalent is `#`. Neither form touches the branch.

The **secondary** GitHub issue is a separate per-slug record attached with `--gh <n>` (typically after a spec/breakout issue is created mid-work) and detached with `--clear-gh`. The TUI's `i` key and `y i` yank prefer the primary tracker URL and fall back to the attached GitHub issue when no primary URL is available; `I` / `y I` always target only the primary. `<slug>` also accepts a branch name.

Bare `wt issue <slug>` prints both, and marks the primary `(set)` or `(from slug)` — the only question the command exists to answer is whether clearing the override would change anything. Both ids appear in `wt ls --json` (`issue_id`/`issue_url` — resolved, not slug-parsed — and `gh_issue`/`gh_issue_url`).

## Stacked PRs

### `wt restack [<branch>] [--onto <ref>]`

Rebase the stack containing `<branch>` (default: the current worktree's branch) onto its updated parents — see [stacked-prs.md](stacked-prs.md). Fetches, reconciles each member's fork-base record against landed PRs (a merged parent reparents its children, anchors preserved), then squash-safe-replays every member onto its parent, force-pushes (skipped for branches with no origin counterpart), and retargets PR bases. A standalone worktree is just a one-member chain: it rebases onto its recorded base, or plain trunk when there's no record — so this (and the TUI's `R`) works on every worktree, not only stacks. `--onto <ref>` overrides the trunk the roots land on.

On a merge conflict it exits 3 and names the failing branch + backup branch — `wt` never auto-resolves conflicts; the `/restack` skill (or you) does.

### `wt restack prune-backups [--days <n>]`

Delete the engine's `backup/restack-*` branches older than `--days` (default all).

### `wt skills [status|sync|diff|reset]`

Keep wt's bundled agent skills (`wt`, `restack`, `manager`, `start`, `handoff`, `triage`) and the managed instructions block installed and current across every harness on the machine — following symlinks, deduping shared directories, and writing through rulesync pipelines (durable source + regenerate) where one manages the target. See [skills.md](skills.md) for the full model.

- `wt skills` / `status` — freshness of every unit at every target, plus remembered template answers.
- `sync [<name>...]` — interactive install/update; the same flow the TUI runs at startup. `--yes` accepts all missing/outdated units without prompting (never touches modified copies); `--force` additionally allows overwriting modified copies. Naming a unit explicitly overrides a remembered decline. `install` is a legacy alias.
- `diff <name>` — what a sync would change, as a unified diff.
- `reset [--answers|--declines]` — forget remembered template answers and/or declined updates.

`wt remote agent start <slug>` uses `skills sync --yes` as worker provisioning
before forwarding the start. That installs missing/updates managed bundled
skills plus instructions without overwriting modified copies; `agent start`
then refuses to submit the harness-native command if the selected harness still
cannot resolve `start`.

### `wt update [log] [--check] [--head]`

Update wt itself. The install is a git clone (see the README), so updating is a fast-forward: `git fetch`, `git merge --ff-only`, and a `bun install` when the dependency manifest changed across the jump. Two safety layers ride along (semantics: [updates.md](updates.md)): the target is the newest incoming commit whose **CI is green** (red/still-running commits are held back; missing checks and API failures fail open), and the result is **boot-probed** in a child process — a version that fails the probe is reverted and skipped until origin moves again. Prints the incoming commits before applying and names any still-running wt instances afterwards; they keep the old code until restarted. Refuses to touch a clone with local changes or unpushed commits; update those by hand with git.

- `log` — print the update/rollback journal plus current / last-good / skipped shas.
- `--check` — only report whether an update is available; don't apply.
- `--head` — ignore the CI gate and target origin's tip.

The TUI runs the same check at startup, before the terminal is taken over: at most once a day, prompting y/n, with a "no" remembered per offered version — it never re-asks until the offer changes. Skipped silently when the clone is dirty/ahead (a wt being developed updates itself by hand). An accepted update restarts an installed events daemon, then re-execs wt so both long-lived processes run the fresh code. A daemon restart failure is reported but does not prevent the TUI from starting. `[update] startup_check = false` ([configuration.md](configuration.md#update)) disables the startup check; `WT_UPDATE=off` disables the whole update system for a single run (the probe harness arms this). The check's daily stamp, journal, and remembered declines live in `~/.cache/wt/update.json`, shared machine-wide like the skills memory.

### `wt rollback [<ref>]`

Reset the wt source clone to a previous version — by default the last one that completed a healthy boot (see [updates.md](updates.md)). Syncs dependencies across the jump, journals the move, and skips the abandoned version in future startup offers until new commits land on origin (`wt update` can always re-apply it explicitly). Refuses dirty/ahead clones. wt also *offers* this automatically: when a freshly-updated version crashes (default yes) or when a previous start of it never finished booting (default no).

### `wt version`

Print the running version — the source clone's git short hash and commit date (`98d1250 (2026-08-08)`), with a `-dirty` suffix when the clone has local modifications. Notes when origin is ahead as of the last fetch (touches no network; `wt update --check` does the live comparison). Also available as `wt --version` / `-v`, and shown in the title of the TUI's help overlay (`?`).

## Integrations

### `wt events <sub>`

The launchd agent is shared per user. `start`, `stop`, `restart`, and `uninstall`
require its installed config and log paths to match the current repository;
run them from the owning repository. `install` explicitly replaces ownership.
Config paths are persisted as absolute paths. See [GitHub events](github-events.md).

The optional GitHub webhook daemon — see [github-events.md](github-events.md).

| sub | what it does |
|---|---|
| `install` | write the launchd agent + generate the HMAC secret; prints the values to paste into GitHub's webhook settings |
| `start` / `stop` / `restart` | load / unload / unload-and-reload the launchd agent; `start` and `restart` also rewrite the agent when the stored one no longer matches the environment (a `brew upgrade bun` makes the baked interpreter path unexecutable) |
| `status` | liveness, bind address, pid, delivery count, last fetch/error, snapshot age, and a `build` line when the daemon is running older code than the caller; during restart warm-up it reports the current daemon and ignored previous snapshot separately (see [github-events.md](github-events.md#the-daemons-build-and-why-the-tui-checks-it)) |
| `secret` | generate or show the HMAC secret |
| `uninstall` | unload + remove the launchd agent |
| `serve` | run the daemon in the foreground (what launchd invokes) |

### `wt agent <sub>`

Message wt-owned coding-agent targets from scripts or another session.

`send` addresses worktree slugs, branch names, and the special `wt`, `main`,
`dotfiles`, and `manager` sessions. It selects a LIVE harness at that target and
falls back to the `Shift+Tab` primary only when nothing is running there. If
several are live, the primary wins. Messaging has no harness override; stale
`--harness` callers fail instead of forcing the wrong session.

That routing rule is the whole point of the command. The primary is one
repository-level setting (`<cacheRoot>/harness.json` — "what F12 would spawn
next", falling back to `[harness].primary`), not a per-worktree one, and routing
by it alone sent three fleet messages to a Claude
session that `send` cold-started for the occasion while the Codex session wt had
itself launched in that worktree got none of them. Because `send` cold-starts,
the failure *creates* a plausible-looking session instead of erroring, so every
send reported success. Hence every send now prints which harness it chose and
why, and a tmux probe that cannot be answered says so out loud rather than
quietly reading as "nothing is live".

| sub | what it does |
|---|---|
| `send <target> [text...]` | ensure the target's selected harness session exists, then submit text; reads stdin when no text args |
| `start <slug>` | ensure that session exists and invoke the bundled `start` skill using that harness's native prefix (`/start` for Claude, `$start` for Codex/OpenCode) |
| `ls [--json]` | list the complete address book with target kind, cwd, live harnesses, selected harness, and whether selection was live or the primary fallback |

Both commands are fire-and-forget with respect to the receiving agent's work,
but fail when delivery is known not to have reached the conversation or its
durable harness queue. Codex output distinguishes started, queued, and the
race-safe queued-or-started receipt. Use
`wt claude` below only for Claude-specific session inspection and control.

For Codex, `send` wakes the tmux session first and addresses its exact UUID
through Codex's durable queue. A busy session or one showing an approval or
question keeps the message queued until it is ready. The direct Unix socket is
local to the target host; remote sends run this same command over SSH. If the
user-managed daemon is offline, wt uses `codex queue` against the same local
store. It never retries an ambiguous queue write.
If a live Codex tmux slot has no recoverable UUID, `send` falls back to typing
into that exact slot and reports that terminal transport was used. Codex slash
commands such as `/compact` also use guarded terminal input because its socket
queue treats them as ordinary text; `$skill` prompts continue through the
durable queue.

### `wt codex selftest`

Read-only diagnosis of Codex messaging. Checks the CLI version, the durable
`codex queue` fallback, and the app-server control socket when it is online.
The daemon is optional and user-managed; wt neither starts nor restarts it.

### `wt claude <sub>`

Inspect and control Claude Code sessions. New callers use `wt agent` for
messaging; the old `send` spelling is a compatibility alias and cannot force
Claude.

| sub | what it does |
|---|---|
| `send <target> [text...]` | deprecated alias for `wt agent send`; resolves the target's active harness and does not force Claude |
| `ls [--json]` | list slugs with a live Claude tmux session. `--json` adds `session_id`, `pid`, `cwd`, `socket_path`, `transport`, `tmux_session`, `status`, `waiting_for`, `busy`, and `last_activity` |
| `selftest [<slug>]` | check that prompt injection still works against live sessions (one line each; nonzero if any fails). This is what tells you a Claude Code update moved the injector's structural anchors — `wt doctor` runs it too |
| `stop <slug>` | stop the target Claude session without typing into its pane (`kill` remains an alias) |

---

There is also an internal `wt _destroy` entrypoint that `rm --background` / `clean` spawn for background removals — not for direct use.
