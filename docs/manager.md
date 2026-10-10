# The manager session

A singleton fleet-coordinator session: one persistent AI harness conversation whose job is the *fleet*, not any single worktree — triage what needs the human, nudge stalled worktree agents, plan merge order, answer fleet-level questions from workers. It complements the [work-status](cli.md#wt-status-slug-state--m-note---risk-r) system: statuses answer "what needs me" at a glance; the manager is for the judgment work above that.

Deliberately thin: wt ships no manager-specific engine. The manager is an ordinary session slot (like the `,` / `.` / `/` slots) named `manager`, running in the main clone, whose *role* comes from its playbook (a skill in your harness config) plus the things wt points at it.

One identity subtlety: the manager shares the main clone's directory with the `.` slot, and Claude's primary-conversation UUID is derived from the directory — so the manager lives as a **named** claude session (`manager~manager` in tmux) with its own deterministic conversation. All the entry points below carry that name automatically; a leftover primary-form `manager` session from before this scheme is killed once at TUI startup (it was literally the same conversation as `.`).

Codex separates these slots using a fixed opening user message saved in each new conversation. The manager's opening message initializes the manager skill and waits for a request; the main slot's message waits for coding work. Discovery, activity, and output readers recognize that opening message before assigning a `primary` name, so `m` and `.` cannot select each other's conversations. Resuming does not send the opening message again. Shell entry (`wt manager`) and detached cold starts resolve the same manager-owned primary thread.

On upgrade from cwd-only Codex discovery, existing unmarked conversations remain available under `.`. The first `m` creates a new dedicated manager conversation; old conversation files are preserved. Restart the wt TUI to load the new discovery code. Existing live sessions are not stopped or reassigned.

## Entry points

- **`m`** in the TUI attaches it (F12 detaches back), creating it on first use with the Shift+TAB-selected primary harness.
- **`M`** opens the [command palette](#the-command-palette-m) — push a canned play (or free text) into the manager without attaching. (Auto-merge, which once lived on `M`, is now the `! m` picker row.)
- **`wt manager`** attaches from a shell; **`wt manager send <text…>`** sends a message, cold-starting the session detached if needed. It is the single outbound channel for worktree agents and scripts, and it stamps the sending worktree's slug on the message automatically.
- **`[[actions]]` with `target = "manager"`** send their rendered prompt to the manager instead of the worktree's session, prefixed `[re: <slug>]` so the subject is explicit. Combined with [automations](automations.md), that's how wt briefs the manager hands-free:

```toml
[[actions]]
id     = "brief-manager-needs-human"
name   = "Brief manager: needs human"
prompt = "{{slug}} asserted needs-human. Read `wt status {{slug}}`, triage: if you can unblock it yourself (gh operations, fleet knowledge), do so and set the next status on its behalf; otherwise summarize what the human must do."
target = "manager"

[[automations]]
id  = "manager-triage-needs-human"
on  = "status.needs_human"
run = "brief-manager-needs-human"
```

Manager briefings (like `builtin:notify`) bypass the automation quiescence gate — they don't touch the worktree, and the interesting fires happen exactly while the worktree's session is busy.

They also never fire on the manager's *own* status writes. Triage ends by sharpening the `needs-human` note, which re-asserts the state and would otherwise brief the manager about itself; the work-status record stamps who asserted it (`by`) precisely so the engine can tell an escalation from an echo. Details in [automations.md](automations.md#a-briefing-never-echoes-its-own-audience).

## The command palette (`M`)

`M` opens a picker of manager plays, built from the same two-screen machinery as the `!` action picker (letter quick-picks, an extras screen before launch, `M` re-press / Enter confirms). Builtins, in order:

| key | command | what it asks for |
|---|---|---|
| `d` | Digest: what needs me | ≤5 bullets — what needs the human now, what's mergeable in what order, what's stalled |
| `t` | Triage needs-human rows | unblock what it can itself, re-assert statuses, distill the remainder to one ask per row |
| `o` | Plan merge order | concrete order + conflict risks + forced restacks |
| `n` | Nudge stalled workers | pointed `wt agent send` to quiet working/review rows |
| `a` | Audit work statuses | cross-check every assertion against PR/CI/session reality, fix drifted records |
| `s` | Start next todo | pick the highest-value `todo` row(s) and kick their agents off |
| `r` | Ask about selected row | free text about the list-pane selection, delivered `[re: <slug>]` |
| `m` | Compact manager context | native `/compact` (no extras screen); one terminal command for Codex |
| `c` | Custom message… | free text to the manager, fleet-scoped |

Codex does not accept inline `/compact` arguments: even literal typing turns
`/compact focus` into an ordinary chat turn. Manager and special-slot palettes
therefore submit bare `/compact` once through tmux, with one Enter and no
preparation message, queue round trip, or preparation-receipt wait. Codex does
not receive the palette's optional date/focus instructions; Claude and OpenCode
retain their inline instructions. The final locked readiness check verifies
thread ownership and an empty idle composer, protecting drafts and approval
prompts. A command is never automatically retried after submission.
Command submission does not prove native compaction completed.

`python3 scripts/native-codex-compact-smoke.py` runs installed Codex in an
isolated tmux session with a loopback fake Responses provider. A Rust bridge
calls the production `CodexMessenger` fallback, including its exact-UUID and
idle-composer readiness checks. The smoke requires one persisted compaction
event and no additional user turn; it tests transport and lifecycle, not
summary quality. No live sessions or credentials are used.

`python3 scripts/native-codex-palette-smoke.py --binary target/debug/wt` runs
the native TUI in an isolated PTY, answers its bounded OSC color query with
fragmented replies and an interleaved navigation key, then verifies the saved
colors reach a private tmux session and the actual Codex composer. It uses
isolated homes and repositories, with no credentials or model requests.

Fleet-scoped commands (`d`/`t`/`o`/`n`/`a`/`s` and custom text) send with no row context and no `[re:]` prefix. The row-scoped entries (`r`, plus any of your `[[actions]]` with `target = "manager"`, which also appear in the palette) launch against the row selected when the palette opened — grayed out when there isn't one.

**Reporting back.** Every fleet builtin's prompt ends with the same contract: finish by running

```
wt manager report [--ok|--warn|--err] "<one or two lines>"
```

The report lands on the TUI's **attention feed** (source `manager`, with a toast) via a watched spool file — so the human sees the outcome of a palette command without attaching, and a missed toast is still in the pane record. Reports written while no TUI is running are not replayed at the next boot (stale triage isn't news); the daily log keeps the durable copy of everything that surfaced.

**Context %.** The footer shows the manager conversation's context occupancy immediately left of `[m]` (from the session tail's per-turn usage; dim, warn ≥70%, red ≥85%). Claude auto-compacts in the low 90s, so red means "run `M m` now, on your terms". The number appears once a live manager claude session has produced a turn.

## The manager's toolbox

Everything is ordinary CLI surface, so any harness can drive it:

- `wt fleet --json` — **the primary sense**: one audit command joining each worktree's asserted status with reality — session liveness (`busy`/`last_activity`) and PR truth (number, draft, merge state, mergeability, CI rollup) — from a single batched GitHub query, recently-removed rows appended ([cli.md](cli.md#wt-fleet)). Rows also carry `section`, the human's manual TUI grouping — treat a name like "Merge after Release" as asserted merge-ordering intent, on par with a status note. `work.by` names who asserted the status: the worktree's own slug normally, `manager` when triage did, `null` for the human — which is how "already triaged" is readable at all. Note the path: nested `.work.by` here, flat `.by` on `wt status --all --json`, and a query against the wrong one answers `null` — the same `null` that means "unattributed". Rows also carry `base` — the effective merge target, never null — and `edges`, the merge edges touching that slug in either direction with `stale` computed: the stack and its ordering, in the row, so building a merge order needs no second command. A `base` naming another row's branch IS a chain. Merge fields read `"computing"` while GitHub lazily calculates; re-run, never poll.
- `wt status --all --json` — the status-only view (state, risk, note, staleness per worktree), plus recently-removed rows (≤48h) so an all-merged fleet doesn't read as an empty one. `kind` discriminates on all three appending surfaces (this one, `wt fleet --json`, `wt ls --json`) with the same values: `"live"` for worktrees that exist, `"merged"`/`"removed"` for history, and only live rows carry `state`/`risk`/`note`. Filter on the value rather than counting rows against `wt section ls` or `wt doctor` — those list live worktrees only, so they legitimately disagree, and reading that gap as a failed prune costs a cross-check every time.
- `wt status <slug> <state> …` — assert on a worktree's behalf after acting on it (`--note-only` sharpens a note without touching state or timestamp).
- `wt edge <from> <before|conflicts|enables> <to> [--blocks|--prefer] [-m why]` — record merge sequencing as structured state instead of prose ([cli.md](cli.md#wt-edge-from-kind-to)); `wt edge --json` reads it back with staleness computed, and each endpoint's `wt fleet --json` row carries the same objects inline. Edges self-expire when either branch moves — re-assert what still matters, never audit the list. Worktrees assert their own first-hand dependencies; cross-branch edges are yours to assert.
- `wt dev queue` / `wt dev queue <slug> --first` — the dev-slot wait queue, and the one lever for saying a worktree goes first. Promotion edits that waiter's own entry, so it takes effect on the waiter's next poll with no message and no cooperation; asking an agent to stand aside instead loses to a slot that frees instantly (it did, once, with three agents all cooperating correctly). Only a queued worktree can be moved (`wt dev start --wait` first), the tier lasts exactly as long as that wait, and worktrees cannot promote themselves — this is the fleet call they are told to bring here.
- `wt agent send <target> "<text>"` — nudge a worktree, branch alias, or the `wt`/`main`/`dotfiles`/`manager` special sessions. wt chooses that target's active harness, or the configured primary when none is active, and cold-starts it. `wt agent start <slug>` invokes a worktree's bundled start skill with the selected harness's native prefix. For a remote worker, use `wt remote agent start <slug>`.
- `wt agent ls --json` — the matching harness-neutral address book, including special sessions, active harnesses, selected harness, and selection source. An inaccessible tmux registry fails closed rather than appearing empty.
- `wt claude ls --json` / `wt claude selftest` — Claude-only diagnostics. The deprecated `wt claude send` spelling delegates to `wt agent send` and cannot force Claude.
- `wt manager report [--ok|--warn|--err] "<text>"` — surface a terse result on the TUI's attention feed (the palette's report-back channel).
- `gh` — PR state, merges (only when the human asked), CI.

### wt owns session addressing and delivery

Callers address every worktree and special slot through `wt agent send`; `wt
manager send` is the convenient manager alias. They never select a harness or
use a harness-private peer name, socket path, or tmux pane.
wt maps the canonical cwd and managed name to a stable conversation identity,
discovers a live process, and cold-starts it when absent. Tmux remains the
process and interactive UI host.

**A cold start that finds a stuck session recycles it rather than failing.** A tmux session can exist with no live Claude process in it (a harness that never came up). tmux refuses a duplicate name, so the start adopts that session, creates nothing, and waits out the registration timeout — and so does every retry, which is why the failure used to be sticky and only `wt claude stop <slug>` cleared it. Now an *adopted* session that still hasn't registered after the full timeout is killed and recreated once, since by then no conversation can be at stake and the concurrent-creator race the adoption path exists for has already lost its whole window. A session this call genuinely created is not recycled: that is the harness failing to start, and recreating it reproduces the failure. Either way the error quotes the pane, which is where a refusing harness explains itself and the only place that says so — the wrapper's `.err` file is empty in every observed instance, and `wt logs` is about destroy logs.

Messages are also **signed**: a send from inside a wt harness session is prefixed with that session's slug (`[eng-1234-thing] …`), from the `WT_AGENT` variable wt stamps at spawn. Agents used to be told to do this by hand, which is the kind of rule that gets forgotten precisely when attribution matters. A harness command (`/…` for Claude, `$…` for Codex/OpenCode) is never stamped — a prefix would stop it being a command.

## How a message reaches a session

wt uses each harness's native input boundary and keeps tmux as the visible,
surviving UI host.

For Codex, wt wakes the exact tmux slot and queues ordinary messages and
`$skill` prompts by the authoritative thread
UUID. With the app-server daemon online it opens a short-lived local Unix
WebSocket, adds the message to Codex's durable FIFO, explicitly starts it when
idle, and disconnects. It never resumes or subscribes to the thread, so the TUI
remains the only owner of questions and approvals. Busy and blocked turns keep
the prompt queued. If the daemon is offline, `codex queue` writes the same
host-local queue. If a live tmux slot has no recoverable UUID, wt waits for
that exact slot's empty composer and types there instead of dropping the
message. A remote send runs on the remote host over SSH rather than
forwarding a socket. An uncertain add is reconciled by its client id and is
never blindly retried. A `/command` uses guarded tmux input instead: the app
server queue accepts it as user text rather than executing Codex's TUI command.
`wt codex selftest` checks the native queue surface without sending.

If a connection fails while reconciling a lost add reply, delivery stays
ambiguous even when the new failure happened before that connection wrote
anything. Once reconciliation finds the receipt, a later start failure retains
the accepted state (`queued-or-started`) and cannot trigger a second submission.

**Sandboxed callers.** wt inherits the caller's OS sandbox; launching a subprocess does not move it
onto the host. A Codex `workspace-write` session can edit repository files
while its effective policy keeps `.git` read-only. `git fetch` still writes
`.git/FETCH_HEAD`, so that policy requires the harness's supported host-execution
approval path. An allow rule authorizes that path; it does not make the
sandboxed attempt writable. Networking and additional writable roots are
separate settings from protected Git metadata.

Manager delivery also needs host-local sockets and Codex state. If `codex queue`
exits with its explicit embedded-app-server startup permission denial, wt reports
`message not submitted` and points to the host-execution approval path. It does
not elevate itself, change permissions, or fall back to typing. Unknown failures,
timeouts, and contradictory receipts remain ambiguous: inspect delivery before
retrying, because the message may already be queued. A PATH-alias warning alone
does not prove startup or delivery failed.

A fatal local-state SQLite `unable to open database file` error also explains
the supported host-execution path for sandboxed callers. It does not establish
a permissions cause or change delivery certainty: inspect delivery before any
retry. wt preserves the original diagnostic and never repairs or changes the
Codex state directory itself.

For Claude, wt submits the message **at the target session's own prompt**, in
its own process. Every Claude session wt starts is launched under
`BUN_INSPECT=ws+unix://<cacheRoot>/insp/<tmux name>.sock`, which exposes bun's
inspector on a private 0700 socket; delivery connects there, walks the live
Ink/React tree to the prompt component, and calls the same `onSubmit` a
keypress would.

That gets four things at once:

- **It arrives as an ordinary user turn** — recorded `origin: {kind:"human"}`, `promptSource: "typed"` — not as peer-framed text carrying a "not typed by your user" preamble. That framing was not cosmetic: it made receiving agents stop and re-ask the human for approval on flows the human had already approved, which is the opposite of what a fleet is for.
- **Slash commands run**, because running one is exactly "submitted at the prompt".
- **A draft in the target's input box survives** — it is read, then re-asserted after the submit clears it, caret position included.
- **A busy target queues it** in Claude and runs it when the current turn ends, exactly as typing would. If Claude is asking the human a question or showing a permission dialog, wt holds the prompt in its serialized per-session queue and submits it only after the dialog closes.

The mechanism is ported from [unseamless-coop](https://github.com/micthiesen/unseamless-coop)'s fleet scripts. Its anchors are structural React props rather than minified names, so they survive Claude Code's minifier churn — but not an arbitrary restructuring. `wt claude selftest` (and the `messaging` banner in `wt doctor`) verifies them and says so out loud.

**Fallback.** If the session has no socket (started outside wt, or before this feature), the socket is stale (the session restarted), or the prompt isn't reachable, wt falls back to typing into the pane — bracketed paste plus the submit keys — and raises an attention line naming which failure it was, because the remedies differ. `WT_INSPECT=off` forces the fallback for A/B-ing a suspected regression.

The cause rides on the send result (`fallback` on a `terminal` result) rather than living only in the log, and one function — `fallbackAdvice` — renders it for both. Two rules hold there:

- **Nothing that merely degraded is reported as broken.** `WT_INSPECT=off` and a harness with no injector at all are fallbacks by construction; they raise no attention line and their advice names no remedy, because nothing is wrong.
- **A machine-level cause is checked before a per-session one is asserted.** "No socket" has a cause that takes out the whole fleet at once: a shim for a harness binary in the PATH shim dir (`<cacheRoot>/shims/`) strips `BUN_INSPECT` from every session at launch, so no session ever binds a socket and restarting one changes nothing. `staleShims()` is the cheap test — deliberately narrowed to those binaries, since the rest of that directory is discovered from PATH and a leftover shim for an uninstalled bun CLI is inert — and both `fallbackAdvice` and the `wt doctor` messaging banner run it before falling back to the age explanation. This is not hypothetical: a `claude` shim removed from the source in `4eda658` survived on disk, and for a day both diagnostics answered "started outside wt, or before this version — restart it" to a failure no restart could fix, 374 times. When one cause explains 100% of sessions, it is not a per-session cause.
- **The advice is never an imperative.** Whoever reads it is usually not the target's owner — an agent messaging a peer, the manager fanning out a briefing — and the target is usually mid-turn, and the message it is attached to was *delivered*. "Restart it from wt to fix" read as an instruction to kill a live conversation to repair something that hadn't failed. Each line states the condition under which direct delivery returns and leaves the restart to whoever owns that session.

One case has **no** fallback, because typing would be worse than failing:

- The submit was sent and went unacknowledged: closing the socket doesn't cancel it in the target, so typing the same text could double-submit. wt confirms against the transcript instead.

A session blocked on a human (`waiting`, e.g. AskUserQuestion or a permission prompt) is queued instead. The active sender keeps the cross-process session lock and polls the native registry with an interruptible async wait; a dialog that appears during injector readiness rejoins the same wait. This prevents the submit key from answering the dialog and keeps later senders serialized behind it. Interrupting the originating CLI or TUI fiber cancels its queued send and releases the lock.

**Confirmation reads back as far as the send, not a fixed number of bytes.** The match is bounded in time (nothing older than the send counts), so the read has to be too. It wasn't: it reused the 64 KiB summary tail, and on a busy session the landed record scrolls out of that almost immediately — measured at **124ms**, because the next record was a large tool result. wt never resends on its own, so the cost lands on the sender, who is told the message isn't in the transcript and that a resend may duplicate. It duplicates. For a message that asks for an action rather than reporting one, that is a double execution.

**Security.** The inspector socket is an in-process code-execution surface for anything that can open it, and its only access control is the containing directory's `0700` mode (re-asserted on every use, since `mkdir`'s mode applies only at creation). The transport it replaced also required a capability token, so this is a real reduction in defense-in-depth — accepted because bun's inspector protocol has no auth layer to hook, and because anything running as this user can already reach the agent's files and credentials directly. `wt` validates the WebSocket handshake before running anything through it, so a different process squatting the path isn't silently trusted.

## Feedback channel (opt-in)

`[manager] wt_feedback = true` permits new actionable evidence for the wt owner.
It does not authorize automatic papercut forwarding, progress reports, or
acknowledgment chains. Verify that a fact is current and changes the recipient's
next action before sending it. Off by default.

## Transient maintenance holds

Use [`wt hold`](cli.md#wt-hold) for a bounded resource window owned by one agent.
`wt agent send <target> --hold <id>` (also `wt manager send --hold <id>`) checks
the existing reference and sends its original scope, owner, event time and
deadline. Sending never extends the window. The recipient must run the embedded
`wt hold check <id>` before acting: native harness queues can deliver it after
release, expiry or replacement. A successful read with `active: false`, an
unknown ID, or a failed read grants no new hold. It is not proof a tool is healthy.

Set/release write state without sending messages. Release watermarks survive
expiry and prevent an older set from resurrecting a freeze. New active holds
cannot displace another active owner or silently extend the same owner's window.
Only operations conflicting with actual maintenance pause; unrelated work and
merges continue. Recheck resource existence and active execution rather than
waiting on retained sessions. Record checks owed during broken-tool repair.

This protocol does not retract prompts already accepted by a harness, parse
free-text freezes, or mutate work-status/merge gates. Ordinary messages and
acknowledgments carry no transient-hold authority. Native queue receipts still
describe delivery, not current applicability or execution.

## Lifecycle

The session survives wt restarts by construction (it lives on the wt tmux server) and is whitelisted from the orphan reaper like the other slots. It is not auto-spawned at boot: the first `m` / `wt manager` / send creates it. Keep its context lean. The playbook should mandate terse replies and periodic `/compact`; the durable fleet state lives in wt (statuses, PRs), never in the manager's conversation.
