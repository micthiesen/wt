# TUI guide

Run `wt` with no arguments to open the terminal interface. Press `?` for the
live keymap; use `/` to filter help. The help text is rendered by the native
application and is the most current reference.

## Layout

The list takes 44% of the width, clamped to 32..52 columns, and the right
column holds details above the activity feed. Details get at most 20 rows; the
feed gets the rest and at least 7. Below 60 columns the panes stack vertically.
With `[ui].activity_pane = "full_width"`, list and details share the top region
and the activity feed spans the bottom with at most 12 lines of content, so a
taller terminal grows the list and details instead. Terminals smaller than 20 columns or 5
rows show a resize prompt. The UI uses a Nord-derived palette and Nerd Font
glyphs; a terminal font without Nerd Font symbols shows placeholder boxes.

Each section starts with a dim rule naming it, separated from the previous
section by a blank line. A row shows, left to right: the stack rail (tinted per
stack lane), the work-status marker, the title exactly as named (issue
identity appears in details, never as a title prefix), a dim `→ Section` when a stack member sits apart
from its parent's section, and a right-aligned glyph cluster. The cluster's
fixed order is action running, dirty, rebase or conflict, deploy, AI session
state, review bot, review decision, PR state or merge-queue position, and CI
checks. `[ui].hidden_badges` removes slots by name (`action`, `dirty`,
`rebase`, `deploy`, `session`, `review_bot`, `review`, `pr`, `checks`).
Archived rows render dimmed. Review requests and the archive sit at the bottom
of the list. The cursor keeps three rows of context while scrolling and the
list border carries a scrollbar when rows overflow.

Work-status markers use color for state: red for needs-human, yellow for
needs-testing or blocked, cyan for working, purple for review, green for ready
or verified, and a dim hollow dot for todo or unset. A slash marks a gated or
dropped status. A hollow marker in a state's color means the saved status SHA
differs from the current observed HEAD, and overdue post-merge verification is
red. A landed branch without a newer status shows the green merge glyph. Landed
means the branch's own commits reached the base: its PR merged, or its HEAD is
in the base and was either merged in or committed on the branch itself. A
worktree only fast-forwarded (or reset) to a base commit is never landed, even
when its recorded fork anchor is older. The same
answer drives the glyph and `c`, and `c` never offers a row whose status is
working or needs-human.

Sections are explicit: one lasts from creation (`l n`) until you delete it with
`D`, and stays on screen when its last worktree leaves, drawn as its rule over
a dim `empty` line. That line is the section's cursor stop, so an empty section
can be renamed, deleted, moved with `J`/`K`, or filled (`n` creates into it).
Deleting a section that still has worktrees asks first and moves them to the
Inbox. The Inbox and the archive appear only when they have rows.

Sections fold with `Tab`. Only folded and empty sections are cursor stops; an
expanded section is just its rows. A folded header reads `[×NN] Name` with a
right-aligned count per work state, most urgent first (ready, needs-human,
needs-testing, review, working), plus overdue verifications, conflicts, and
failing checks. Selecting it shows the full rollup and blocker notes in the
details pane. `[ui].sort = "status"` orders rows by work status; manual section
placement remains separate. Within a section, rows without a manual order sort
first, and a stack sorts as one unit under its root's identity.

The details pane's border names the slug. It starts with the wrapped title
and a dim tag naming its source (see [Worktree titles](#worktree-titles)).
The work-status block follows when a status is saved: state,
risk, age, and commits since the status, then the gate and the note behind a
rail in the state's color. `OPS`, `REVERT`, `IF WRONG`, and `UNTESTED` note
fields align under a hanging indent. A verify-after-merge obligation shows its
step count with a two-line preview; `V` expands or collapses the steps, and
they start expanded when the check is due.

Labeled rows follow, with dim right-aligned labels. `[ui].rows` controls their
order and visibility: `branch` (with its landing base), `issue` (also
`linear`), `stage`, `dev`, `pr` (number, merge queue or auto-merge, checks with
failing names, review, review bot, and with `[guides] enabled` the change
guide last), `claude` (every Claude, Codex, OpenCode, and Pi
session, the F12 target first), and `git` (dirty or clean, diff against the
merge-base, last commit and creation ages, upstream counts in parentheses, base
counts in brackets). The optional `path` group can be added explicitly and
keeps the checkout name visible when truncated. Unknown group names are
skipped. On narrow panes the `pr` and `git` rows drop their least important
parts first. A GitHub or host error remains visible as an error, and an
unavailable fact shows `—` rather than reading as clean.

Below the rows come a rebase block when the branch would not rebase cleanly
onto its base (listing up to eight conflicting files from a `git merge-tree`
pre-flight), paused automations, the session summary, PR comments, and the
unresolved thread count. A folded section's details show its rollup, member
rows, a red "blocked on you" group with each needs-human member's note, and a
yellow "blocked on" group with external gates. A review request's details show its state, branch,
author, checks, and keys. The details pane scrolls with `Ctrl+J`/`Ctrl+K` and
shows a scrollbar when it overflows.

Creation uses the section where the name prompt opened, including Inbox.
The new row appears below the selected row or stack. A folded section opens
and receives the new row at the top. This position is stored on the controller,
including for a remote checkout. An unstarted row keeps this position across
restarts. Under status sorting, its work status takes precedence once assigned;
manual reordering also replaces the initial position. An existing worktree keeps
its section when its name is entered again.

Creation immediately selects one optimistic row with its title and a progress
glyph. Normal list navigation, attention controls and default-harness switching
remain available. The
chosen host resolves the exact checkout identity before creation starts, so
inventory replaces that row without a duplicate. Navigation away is preserved
when preparation or creation finishes. Worktree actions, including F12, become
available when the real checkout is ready; F12 is never queued automatically.
Dependency installation continues as a background action. A removal request
keeps the row in place with a dim gray, struck-through name while safety
checks run. When the worker accepts removal, the row moves to **Removing**, directly
above **Archived**, and file cleanup continues in the background. Active
removals reject duplicate requests, and session entry on that row (F10, F11,
F12, Shift+F12, `;` and their pickers) says **Removal is already in progress**
instead of queuing behind the teardown. Failed rows stay in Removing with an
alert marker in place of the trash marker, and the details and attention panels
say **Removal failed** with the error.
Press `d` to inspect the failure and review a retry; the host repeats all
safety checks. If checkout identity is missing, the menu permits inspection
only. When the failed record's checkout is no longer in Git inventory, `d`
instead offers **Dismiss failed removal**, showing the error. Dismissal
forgets only that record: it deletes no files or branches, so delete any
leftover directory by hand. The host refuses it as stale unless the record
is still the same failed job, path and branch, and the checkout path is
absent and unlisted. Removing rows survive restart, even after partial deletion. A row leaves
the list only after the worker confirms success and inventory drops it. A request
cancelled before worker startup restores the normal name style. Delayed hazard reviews are queued separately for each checkout
and host. They appear one at a time after the current menu closes; navigation
does not discard them. Each confirmation still rechecks its checkout witness.
Commands taking more than three seconds show elapsed progress. A queued command
names the conflicting operation or capacity wait; this is progress, not a
completed request. Independent controls do not wait for checkout creation.

### Cached, pending and failed states

At launch every pane shows the last data wt saved for it, so the board is
populated before the first fetch. Each pane's title names its state the same
way: `GitHub cached 3m` while that domain shows saved data, or
`MaxBook GitHub cached 3m` for a remote host in the fleet summary,
`loading…` before any data exists, `↻` while a fetch takes a moment, and
`<source> failed` when a fetch failed (the last good data stays visible; saved
data that also failed to refresh reads `GitHub cached 3m · GitHub failed`).
The age belongs to that saved domain, not the whole board or its last refresh.
A successful read clears its cached marker even when the data is unchanged;
a failed or skipped read does not. Unused tracker answers do not mark the list
stale. The list summarizes every host; a row's details reflect only that row's
host. The top bar does not repeat source freshness labels from pane borders.

Every change you make appears at once. Worktree rows contain their title,
stack references and status glyphs only, never issue prefixes or status text.
One fixed-width pending glyph marks `…` while a request is in flight, `↻`
once accepted, or `?` when its outcome is unknown. Uncertainty takes precedence
when several changes are pending. Full explanations, such as
`? arming merge (unconfirmed)`, stay in the details title. Unknown action
outcomes and expired updates also enter the attention feed. Board-wide changes,
such as switching the primary harness, show progress in the bottom status line.
Uncertain board-wide outcomes stay in Attention, without replacing the top bar.
A failed request
rolls back at once with an error toast. A change that a fresh read still does
not show after its deadline (30 seconds for GitHub, reviews and issue statuses;
12 seconds for local state; 10 minutes for creating or removing a worktree) is
dropped with a notice; nothing is resent automatically. A key that turns out to
have nothing to do (for example, moving a row that is already at the top) ends
its marker at once. Toggles act on what the row shows, and GitHub writes are refused
if the PR or its head commit changed since you saw it.

The bottom pane shows the attention feed, all activity, session output, or
tracked action output; its title names which, such as `attention · 2 new` or
`slug · claude / name · live · 1/3`. Rows show a dim local time, the source,
and the message. Attention carries status changes and section moves made
elsewhere (agents, the manager, another process); your own moves in this TUI,
including rows a deleted section returns to the Inbox, and statuses you set
with `u` appear only as the toast and in all activity. Attention entries wrap with a hanging indent; the full feed is
one line per event, colored by level. Entries already marked seen are dimmed
above a `seen HH:MM:SS` rule; when nothing new follows it and the feed overflows
the pane, the rule takes the bottom row, and an empty attention feed reads "nothing needs
you". The attention watermark is saved and `x` or `X` marks the current feed as seen.
`Ctrl+E`/`Ctrl+Y`, `Ctrl+Shift+J`/`Ctrl+Shift+K` where supported, and the
mouse wheel scroll the output pane. Selecting an output with `'` pins it until
it ends; `[`/`]` cycle outputs, `"` toggles attention/all activity, and `Esc`
returns to the default. The feed is restored from the app log on startup.

The header shows worktree and archive counts, refresh motion, paused or queued
automations, a dim `↑` notice when a newer wt release is available or installed
and awaiting a restart ([updates.md](updates.md)), the primary harness's rate-limit windows (colored as they near
the limit, with time to reset), and the primary harness glyph. Claude's
windows come from `~/.cache/claude-statusline-usage.json`, written by a
statusline outside wt; a reading over 30 minutes old turns dim and ends with
`as of <age> ago`, and a window it saw reset shows `?` instead of `0%`. Refreshing uses
a traveling bar wave in a reserved 12-cell slot, so counts and
right-aligned telemetry stay put as refreshes start and finish. The wave follows
the number of refreshing source lanes across hosts and eases to rest. Its 10 Hz
timer stops when idle or covered by a modal; animation only repaints the header.
The footer shows
the active prompt, a toast (failures in red with a glyph), a source error,
board-wide action progress, or a
static key hint, and on the right the manager, main, wt, and dotfiles slot
sessions as `[m] [.] [,] [/]` colored by session state. The manager Claude
session also shows context occupancy after a transcript turn provides usage.
These are status labels, not clickable buttons.
Physical terminal presence is separate from conversation identity. A live Codex
terminal can show its explicit working or action-required title signal before
wt knows its thread UUID. A live terminal without a reliable activity signal
uses the unknown-state color rather than the absent-terminal muted style. Title signals
never assign history, route messages, or establish that automation may act on
an idle session.

Modals share one frame: a rounded border (yellow for destructive confirms), a
title, and their keys listed along the bottom edge. Pickers show chords or
digits in a key column and a `›` cursor. Confirm hazards are red. Help (`?`)
groups the keymap by area and ends with a glyph legend built from the same
rules the list uses; `/` filters with a match count and highlights hits.

History rows are prepared in the background and retained when the pane closes.
`h` shows them immediately while tracker and landing facts refresh.
When `h` opens removed-worktree history, the list pane reads `removed (N)`
and groups rows under day rules, each day starting at 04:00 local time. A row
shows a marker (the work-status color when a landing is proved, otherwise
merged, closed, dropped, gone, open, or a dim trash glyph), the title, pause,
issue and PR glyphs, and a right-aligned age. Older records can be checked
against a bounded local GitHub-merge and production-history scan while history
is open. A landing only moves forward: once a removal is proven in
production its record keeps that (`landedOn`), so a later refresh or restart
never shows it back at the base. The `removal record` pane shows removal time, host, saved status,
verification, landing, and notes. `Enter` restores, `p` opens the PR, `i` opens
the issue when known, and `y` copies the branch. `j`/`k`, `g`/`G`, and `PgUp`/`PgDn` move the cursor, `Ctrl+J`/`Ctrl+K` scroll the record pane, `r` refreshes, and `Esc` or `h` returns to the board. The activity pane's own keys also work: `x` marks attention seen, `"` toggles the attention and firehose feeds, and `Ctrl+E`/`Ctrl+Y` scroll it. Global keys (help, performance, create, manager and slot sessions, automations pause) work as on the board, and `q` quits the app. `p` and `i` toast when nothing is recorded, and `O`
opens the main clone as it does on the board.

## Keymap

| Key | Action |
|---|---|
| `j` / `k`, arrows | Move the selection; at the first or last item, scroll the list to that edge |
| `g` / `G`, `Home` / `End` | First / last visible item |
| `PgUp` / `PgDn` | Move half a page |
| `Space` | Next row needing you: saved work states plus failing checks, changes requested, unresolved threads, and failed actions; toasts when nothing needs you |
| `Tab` | Fold / unfold the section; unfolding lands on its first row. Requested reviews fold too |
| `Ctrl+D` / `Ctrl+U` | Next / previous section |
| `Ctrl+J` / `Ctrl+K` | Scroll details |
| `Ctrl+E` / `Ctrl+Y` | Scroll output |
| `r` / `Ctrl+R` | Refresh / ask for confirmation, then clear derived caches and refresh |
| `?` | Open searchable help |
| `Esc` | Return the bottom pane to its default feed |
| `q` / `Ctrl+C` | Quit |

### Worktrees and organization

The create prompt shows `New worktree name: ` before an empty editable field.
With remotes configured it shows `New worktree on <host> (Tab: host): `
instead. `Tab` cycles through this machine and each remote, and `n` starts on
the host chosen last in this session. A stacked create (`N`) opens on its
parent's host, and `Tab` does nothing once the text carries `--base`; a
`--base` typed after choosing a host resolves on that host.
A failed create keeps the name and chosen host editable, with the error shown
in red above the prompt.
Submitting selects a pending row immediately. Inventory and metadata replace
that same row once creation finishes; progress stays until then. Its group
opens while the pending row remains selected. Navigating elsewhere prevents
the later creation result from changing selection.

The Archived group sits at the bottom of the worktree pane when all visible
items fit. When the list is taller than the pane, it scrolls with the list.

Clean and delete use the host's existing GitHub data. They do not refetch PRs
when you press the key or confirm. A merged PR must match the current worktree
commit. Local file changes, unpushed work, and checkout identity are checked
again before removal. When landing evidence comes only from Git ancestry,
WT still checks the published base. Missing evidence does not mean merged.

| Key | Action |
|---|---|
| `n` / `N` | Create a worktree on the last chosen host (this machine by default) / create from the selected branch on its host |
| `Tab` (create prompt) | Cycle the new worktree's host |
| `Ctrl+N` | Create on a chosen host; toasts when `[remote]` is not configured. Managed hosts that owe a sign-in say so |
| `o` | Open the selected worktree in the configured editor |
| `d` | Remove the selected worktree after confirmation |
| `c` | Open cleanup review from background results, including archived rows |
| `a` | Archive / restore the selected row; restore returns it to Inbox. A locked row is refused |
| `t` | Rename the worktree. A rename is final: no agent or tracker title replaces it |
| `#` | Set or clear the worktree's issue identity |
| `i` / `I` | Open the preferred issue / primary tracker issue |
| `s` | Open the deployed stage or dev URL when available |
| `z` | Open the row's change guide. Only with `[guides] enabled`; see [configuration.md](configuration.md#guides--change-guides) |
| `V` | Show or hide this row's post-merge verification steps (open by default when the check is due) |
| `u` | Set or clear the work-status claim, note, or verification obligation (letters `t w r n h y v d` pick a state, `a` ready with verification, `x` clears, `m` edits the note) |
| `y` | Copy a worktree field: `b` branch, `s` stage name, `S` stage URL, `d` dev URL, `p` path, `n` slug, `i`/`I` issues, `r` PR. Unavailable entries stay listed. On a folded section, `n` copies the section name, `s` the slugs, `b` the branches, `l` a list |
| `l` / `L` | Move to a section (`l l` confirms, `l n` creates a new section; `Esc` returns to the picker) / rename the section. The picker lists every section in board order with its size |
| `D` | Delete the current section; its worktrees move to the Inbox (confirmation when it has any) |
| `J` / `K` | Reorder the selected row, stack, or folded or empty section; the cursor follows the row across sections, including into an empty one on screen, never into a hidden group |
| `b` | Record a fork base without rebasing: another worktree's branch (never the row or its descendants) or none; `b b` confirms |
| `R` | Restack or rebase the selected branch |
| `h` | Open removed-worktree history |

### Worktree titles

wt never generates titles or summaries; a title is only ever set on purpose,
and the latest `wt status` note is the row's running summary. Each saved title
records its source, and a lower source never replaces a higher one:

| Source | Set by |
|---|---|
| `user` | `t`, or creating from the TUI. Final |
| `agent` | `wt new <words>` or `wt title` from an agent. It may replace an `issue` or `fallback` title once, with the outcome its PR will have |
| `issue` | `wt new <ID>` with no title words: the tracker's title (`[issue_tracker] status_command`) |
| `fallback` | The same, when the tracker gave no title: the bare ID |

A worktree nobody titled (created from a branch or a one-word slug) shows its
PR title, then its first commit, then its slug, tagged `pr`, `commit`, or
`slug`. Titles change only the display, never the branch or directory.

Creation selects the row after it appears in the prepared board. Remote
worktrees use the same host service for commands and sessions. On a
[managed remote](remote.md#managed-remotes), a request that needs a missing
sign-in (Codex for agents, GitHub for creation) first asks `Sign in on
<host>`; confirming runs the sign-in over SSH and then continues the request. Removal and
cleanup revalidate current hazards; unknown state does not authorize force
removal.

The `c` menu opens at once from the prepared background board. It does not wait
for Git, GitHub, or SSH. It also requests the same background refresh as `r`.
With no candidates, the overlay stays open and shows
why each row is kept. Candidates appear first with green trash glyphs; kept
worktrees and their reasons are muted under a separate count. Long names and
reasons wrap below their own column, including in the no-candidates view.
Press `c` or `Esc` to close it. Reopen it to use newer
background results. Confirmation checks only the displayed candidate identities
against current landing evidence and removal hazards before queuing removal.

Remote worktree names have a server icon in salmon before the name. Archived
rows use the dim foreground color for this icon.

### Pull requests

| Key | Action |
|---|---|
| `p` | Open the selected pull request at the configured target (also `Enter` on a review request) |
| `g p` / `l p` | Open the PR in GitHub / Linear Reviews |
| `e` | Mark a draft PR ready after confirmation; toasts when it is already ready |
| `E` | Ship after confirmation, listing only the steps still needed; toasts when already shipped |
| `! m` | Toggle merge-when-ready |
| `f` | View failing check logs |
| `v` | Select requested reviewers |
| `i` on a review request | Open its linked issue |
| `w` | Check out a selected review request |
| `d` on a review request | Dismiss that review request |

Merge-when-ready uses the PR's base branch to choose merge queue or classic
auto-merge. `! m` arms or disarms based on what the row shows, so a quick
second press cancels even before GitHub reports the arm. Pending-check retries
retain the expected head SHA. A second `! m` cancels an idle retry; an
uncertain external result (a timeout or interrupted request) is shown as
unconfirmed and never resent; arming again is refused until a read shows the PR
armed or the unconfirmed attempt expires.

### Sessions and actions

| Key | Action |
|---|---|
| `F10` / `F11` / `F12` | Enter shell / diff / agent session |
| `Shift+F10` / `Shift+F11` | Stop shell / diff session after confirmation |
| `Shift+F12` | Start a new agent session: `c` Claude, `x` Codex, `o` OpenCode, `p` Pi, or `F12` for the highlighted one |
| `;` | Pick a session. Inside: `d` closes the highlighted live session gracefully and `x` kills it (a dead Claude row is forgotten), both without confirmation; `c`/`x`/`o`/`p` on a New row jump between harnesses; an empty Claude name picks one automatically |
| `Shift+Tab` | Cycle the primary harness |
| `!` | Open worktree actions: `m` merge when ready, `u`/`g` agent builtins, `d`/`s` dev server, `l` dev logs, `c` custom prompt, and configured actions |
| `m` / `M` | Enter the manager session / open manager commands (`d t o n a s` fleet plays, `r` ask about the row, `m` compact, `c` custom; `M M` confirms) |
| `,` / `.` / `/` | Enter the wt repo / main clone / dotfiles session when configured |
| `<` / `>` / `\` | Open the corresponding special-session palette (`g` continue, `m` compact, `z` open in editor, `c` custom) |
| `O` | Open the main clone in the editor |

F12 opens the existing live agent session even after Shift+Tab changes the
primary harness. It retains the exact session, including a named Claude session.
If several harnesses are live, F12 prefers the primary harness among them.
Use `;` to choose another session. If no session is live, F12 uses the primary
harness. A configured `harness.wt_session` still selects the `wt` special slot.

While session entry waits for a host, `Esc` cancels that connection attempt.
Navigation or another session request also cancels the wait. A cancelled remote
connection does not block entry to a local session.

When a matching remote build is unavailable, session entry stops without waiting
for the connection timeout. A “Cannot open remote session” dialog offers
Dismiss (Enter or Esc) and Copy details (`c`). Dismiss returns control to the
board. The copied details include the local build, remote platform, and release
lookup error, including GitHub 404 when that is the cause.

Most menus open at once from the board on screen: status `u`, fork base `b`,
section move, sessions `;`, start agent Shift+F12, stop a running terminal
Shift+F10/F11, GitHub `e`/`E`, review checkout `w`, history restore, hard
refresh, create, removal of a clean worktree `d`, and an action palette that
was prefetched for the selected row. An open menu updates as newer data
arrives and keeps the highlighted choice. If what the menu showed changed
before the host applied the choice (a PR moved, a session was replaced, an
agent set a status), nothing is written: the menu reopens with the current
data and the toast says what changed.

Menus that need host-only data (removal hazards, an unprefetched palette,
action arguments, reviewers, failed checks, fleet cleanup, retained or remote
terminals) show
`Loading menu… Esc cancels`. Keys typed meanwhile belong to that menu, never to
the board: they replay in order once it opens, so a fast `; d` or `! m` acts
inside the menu. Escape, `q`, Ctrl+C, or the menu's own key cancels it.
Replay stops if the menu closes. Keys are dropped, with a notice, when the
menu fails or takes longer than two seconds. Mouse wheel input still acts
where the pointer is and abandons the loading menu.

The session picker runs on the selected host. A running Codex, OpenCode, or Pi slot
whose conversation UUID cannot be proven appears as **current session
(conversation not identified)**. Enter reconnects to that exact terminal;
historical conversations remain separate. Live selections capture the tmux
session ID and creation time. If the session changes or ends, attachment and
stop actions refuse the stale selection and ask you to reopen the picker.

While attached to a worktree session, F10 opens its shell, F11 opens its diff,
and F12 returns to its agent. Press the key for the current session to return
to the board. Switching between sessions does not require a board visit.
Special slots such as the manager retain their board-return behavior. `/compact`
is available in harness command palettes that support it. Manager and special
session controls remain reachable by these keys. The footer shows live
special-session state and manager context usage when those facts are available.
If a terminal command fails, its output remains visible instead of ejecting you
to the board. Press Enter to retry that command, or press the current session's
function key to return to wt. Commands
that exit successfully close normally.

### Automations and output

| Key | Action |
|---|---|
| `A` | Pause / resume all automations |
| `Ctrl+A` | Pause / resume automations for the selected worktree |
| `Ctrl+Shift+A` | Cancel queued automations |
| `'` | Pick an activity, session, or action output |
| `[` / `]` | Previous / next output |
| `"` | Toggle attention / all activity |
| `x` / `X` | Mark attention as seen |

### Performance and errors

Press `P` for the process and system performance view. `r` takes a sample,
`c` toggles repeated sampling, `i` sends the snapshot to the wt repo session
and enters it, `j`/`k`, page keys, and `Ctrl+D`/`U`/`E`/`Y` scroll, and `Esc`,
`q`, `P`, or `Ctrl+C` closes it. The same snapshot is available with [`wt perf`](cli.md#wt-perf---json).
Command and source failures remain visible in the footer, source state, or
activity/attention feed. The native TUI does not provide a full-screen
uncaught-error recovery overlay.

## Picker and text input

Help, the performance view, and log overlays (such as dev-server logs) share
one scroll keymap: `j`/`k`, arrows, and `Ctrl+E`/`Ctrl+Y` move three lines;
`PgUp`/`PgDn` and `Ctrl+U`/`Ctrl+D` move half a page; `g`/`G` or `Home`/`End`
jump to the edges. `Esc`, `q`, and `Ctrl+C` close them.

Use `j`/`k`, arrows, page keys, and `g`/`G` to move through lists. `Enter`
confirms; `Esc`, `q`, and `Ctrl+C` cancel, and confirmations also cancel with
`n` or their opening key. `Space` never confirms. Repeating the opening key
confirms the current selection. Digits `1`-`9` pick list entries, such as sessions, saved values,
statuses, sections, bases, outputs, and harnesses, but not palette rows,
which use their letters. Reviewer selection
uses `Space` to toggle entries. `Esc` or `Backspace` on an empty sub-prompt,
such as a new section or session name, returns to its picker. Text fields support cursor movement, word movement,
Unicode-safe deletion, and `Ctrl+U`/`Ctrl+K` to clear to the start/end. A
custom `! c` prompt keeps pasted line breaks.

## Mouse

The wheel scrolls the pane under the pointer. Over the list it scrolls the
viewport without moving the cursor; the next cursor move scrolls back to it.
Dragging selects text within one pane and copies it to the clipboard on
release.

## Deliberate behaviors

Several behaviors are intentional rather than incidental:

- The dirty badge stays on landed rows. Cleanup would discard those files,
  so the hazard stays visible until the row is removed.
- Folded section headers show a count per work state beside the row count,
  so a folded group still says what needs attention.
- Landed rows take their title from the landing commit when no title was
  set, rather than repeating the branch name.
- Modals use rounded borders and pane titles are dim, so the focused modal
  stands apart from the panes behind it.
- The perf overlay leads with a verdict on whether wt is the load, then
  meters, a per-category breakdown, and the heaviest processes inside and
  outside wt's process tree.
- Help adds selection, sync notation, and create-prompt flag sections, and
  its search finds keys by their aliases (`/rebase` finds `R` and `b`).
- The create prompt reads `New worktree name:` rather than `new:`, so an
  empty field says what it expects.
- A key with nothing to act on, such as `e` without a PR, shows a yellow
  toast and is not recorded as a failed action on the attention feed.
- Remove confirmation revalidates hazards before removal, and `f` keeps its
  log overlay.
- `Space` jumps through a broader attention set (status, checks, review,
  failed action) and toasts when nothing needs you.
- `Esc` output focus is global rather than per worktree. Errors appear on
  the attention feed rather than in a dedicated error overlay, and `F12` on
  a remote row does not queue an attach.
- The work-status header colors `risk low` / `risk high` by the risk, as the
  list's risk column does, rather than by the state, so a high-risk `ready`
  never reads green.
- A landed row's git line names the base as the branch line does
  (`merged into main`) rather than always saying `origin/main`.

Session `; d` and `; x` act immediately, without confirmation; the
review-requests fold is not persisted.

The session picker is built from the board: saved conversations from the
background history snapshot and the live tmux slots. Opening it sends a
background read, so saved conversations join the open picker when discovery
finishes, without a loading row. With an empty cache it shows live slots and
new-session choices. Attachment rechecks the selected live identity; a
new session chosen beside a live slot the picker did not show is refused and
the picker reopens. A picker cursor that has not moved follows the newest
default, so a picker opened before a live slot appeared selects it once it
does. Shift+F10 and Shift+F11 confirm a running terminal from the board; a
terminal retained after its process exited, or one on a remote host, is read
from tmux by its host.

The `d` delete menu opens from cached worktree data, including dirty and remote
rows. It shows current cached warnings. After confirmation, the host checks
identity and removal hazards. Any hazards or stage destruction require a fresh
confirmation before removal. A missing HEAD shows an information menu only.
