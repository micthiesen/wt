# Hub mode

Hub mode is an opt-in task-inbox UI: instead of wt's three-pane worktree list,
you get a narrow column of prioritized "things that need you" next to a pane
that's always a live coding-agent (or diff/shell) session. Enter never leaves
you staring at a static list — the moment you land on a task, its session is
already there.

Turn it on with `[ui] mode = "hub"` (see [configuration.md](configuration.md#ui))
so a bare `wt` boots into it, or launch it explicitly with `wt hub` regardless
of config. `wt classic` forces the regular three-pane TUI either way. Classic
and hub read and write the exact same on-disk state (wtstate, task-focus
stamps, tmux sessions) — there's nothing to migrate, swap between them
whenever.

## Architecture

Hub mode is two nested tmux servers:

- An **outer server** (socket `wt-hub`, one session named `hub`) that holds
  nothing but a two-pane layout: the left pane (~35 cols) runs `wt _taskpane`
  (wt itself, in task-inbox mode); the right pane is a nested tmux *client*
  into the **inner** `-L wt` server — the same server every classic-mode
  F10/F11/F12 session already lives on. The divider between the panes is
  painted in wt's theme background (tmux can't remove the column, only
  recolor it), so there's no visible bar.
- The **inner server** is untouched by hub mode's existence. It hosts every
  harness/diff/shell session exactly as it does in classic mode. When the
  right pane needs to show a different worktree's session, wt runs
  `switch-client` to retarget that pane's tty at a different inner-server
  session — no attach/detach dance, no renderer suspend, just an instant
  in-place swap (`core/hub/control.ts`'s `switchRight`).

Because the outer server never hosts real work, it's cheap to kill and
rebuild — `wt hub` does exactly that whenever its generated config
(`~/.cache/wt/hub-tmux.conf`) changes. And because harness sessions live on
the inner server regardless, **they survive a hub crash, a `wt` restart, or
`wt hub`'s own kill+rebuild by construction** — there's no special-case
recovery code, the sessions were simply never coupled to the outer chrome.
`remain-on-exit` plus a `pane-died` hook on the outer server auto-respawns
either pane if it dies unexpectedly, so a crashed `wt _taskpane` or a dead
nested client comes back on its own instead of leaving a blank pane.

When the task cursor points at nothing with a live session, the right pane
parks on a reserved `wt-hub-home` session (`wt _home`) — a static dashboard
printing the core key legend.

## The command layer

Typing always goes to the session pane. wt itself is driven by a **⌘
layer**: Alacritty translates each `cmd+<key>` chord into an ESC-prefixed
sequence (see the `wt hub command layer` block in
`~/.dotfiles/alacritty/.config/alacritty/alacritty.toml`), which the hub's
outer tmux server intercepts as an `M-<key>` root-table binding and relays
into the task pane — from either pane, no matter where focus sits. Cmd was
chosen deliberately: skhd/yabai globally own the Alt, Shift+Alt, Ctrl+Alt,
and Hyper spaces (so bare Option chords for `j`/`k`/`n`/`1-5` never even
reach the terminal), while the Cmd domain is free apart from a few
overridden defaults. The overrides are GLOBAL to every Alacritty window
(Alacritty can't scope per-process) — accepted deliberately, in full:
Hide on `⌘H`, Minimize on `⌘M` (both fight yabai tiling anyway), search
on `⌘F` (stays on `Ctrl+Shift+F`), close-window on `⌘W`, and
clear-scrollback on `⌘K` (`Ctrl+L` covers it). `⌘N` is deliberately
untouched — it stays Alacritty's new-window; new-worktree rides `⌘T`
("new task"), whose literal `t` (AI-regen, rare) moved behind `⌘H`.

Most cmd chords forward the bare classic key; five have dedicated rebinds
because the literal letter means something else in classic mode: `⌘H` →
focus the task pane (literal `h` was removed-history, retired in hub),
`⌘D` → diff view (literal `d` is destroy — destroy moved to `⌘⌫`), `⌘S` →
shell view, `⌘F` → zoom, `⌘W` → graceful session close.

Pane focus is not something you manage: `⌘L`/`⌘D`/`⌘S` land focus in the
session; `⌘H` brings it to the inbox; **Esc** in the task pane (no picker
open) bounces it back to the session; pickers and prompts pull focus
automatically and restore whichever pane held it before they opened. The
tasks panel border tints accent while typing lands there and dims when it
goes to the session. Refocusing the task pane while the right pane shows
something other than the selection re-asserts the selection follow.

Rare classic actions (yank `y`, reviewers `v`, base `b`, restack `R`,
archive `a`, clean `c`, AI regen `t`, ready/ship `e`/`E`, CI logs `f`,
review-checkout `w`, zed opens, `Shift+Tab` harness cycle, `Ctrl+A`
automations, `Shift+F10/F11/F12`) have no cmd chord: `⌘H`, type the
letter, `Esc` back.

Outside the hub the cmd chords degrade to ordinary Meta keystrokes (`⌘D`
in a plain zsh is `kill-word`, in a full-screen classic claude attach
they're inert Option chords) — the same class of leak the Option layer
always had, no new failure modes.

## The task model

The inbox is flat: one line-item per "thing that might need you." Three
kinds share the same sort/render machinery (`TaskItem` in
`src/tui/hooks/useTaskRows.ts`):

- **`wt`** — a standalone (non-stacked) worktree, or one member of a stack
  you've expanded.
- **`stack`** — a whole stack, collapsed to a single task. Its bucket/reason
  is driven by the **focus slice**: the member whose derived bucket ranks
  most urgent (ties broken by spine position), so a stack with one member
  needing you surfaces as "needs you" even if its siblings are idle. `Tab`
  on a stack task expands it into one row per member (in spine order); `Tab`
  again collapses it back.
- **`pr`** — a review-request PR with no local worktree at all. These skip
  the bucket-derivation pipeline entirely: a non-draft review request is
  always `needs-you` ("review requested"), a draft is `waiting` ("draft
  review request"); the detail line shows `<author> · +<additions>
  −<deletions>`.

Archived worktrees are filtered out before any of this runs — the inbox
never shows them (classic mode's list still does, via `a`/archived-section
toggling).

## Buckets

Every worktree/stack task gets folded into exactly one bucket by a
first-match-wins precedence ladder (`computeBucket` in
`src/core/task-state.ts`) — pure and unit-tested, no queries or fs inside it:

| bucket | meaning | example triggers (first match wins, top to bottom) |
|---|---|---|
| `needs-you` | a human is required right now | agent is asking a question · mid-rebase conflict · merge conflict with base · CI failing · changes requested |
| `review-output` | a turn ended and you haven't looked since | the freshest session tail ended its turn (`end_turn`) more recently than your last recorded focus of that slug |
| `ready` | approved and green, just land it | open non-draft PR, review approved, checks passing or none |
| `working` | something's actively running for you | worktree busy-locked · a tracked headless action is running · agent session state is `working`/`polling` · auto-merge armed with CI still pending |
| `waiting` | blocked on others | in the merge queue · CI running · awaiting review |
| `idle` | nothing pending | dirty tree, or just quiet |
| `done` | landed | branch merged or otherwise gone — sweep with `c` |

`done` is checked first (so a just-merged PR can't get stuck reporting a
stale `needs-you`), and `mergedOrGone`/`pr.state === "MERGED"` short-circuits
everything else. Within a bucket, tasks sort by most-recent activity.

## Manual states: pin and snooze

Two per-slug flags, persisted in wtstate (`taskPinned` / `taskSnoozedBucket`),
override where a task sorts without touching its underlying bucket:

- **`P`** pins the selected task to the very top of the inbox (own "Pinned"
  section, ahead of even `needs-you`). Toggle again to unpin.
- **`z`** snoozes the task **at its current bucket** — not for a fixed
  duration. It drops into its own "Snoozed" section (ranked between `idle`
  and `done`) and stays there only as long as the derived bucket doesn't
  change; the moment `computeBucket` produces a different bucket than the
  one you snoozed (state moved on), the snooze is stale and the task pops
  back to its real position automatically. `z` again while snoozed
  un-snoozes explicitly.

Both are worktree-backed only — review-request `pr` tasks can't be pinned or
snoozed.

For a **collapsed stack** the group's placement uses an effective overlay
rather than just its focus slice: the stack sorts as pinned when ANY live
member is pinned, and as snoozed only when EVERY member is live-snoozed at
its own bucket — so snoozing one urgent slice can't bury the whole stack,
and pinning any member is enough to surface the group. `z`/`P` on the
collapsed entry still act on the focus slice; Tab-expand to manage members
individually.

## The unread-output bit

Known limitation: the focus stamp is **per slug**, not per session. A
worktree hosting several claude sessions (primary + named) has one clock;
viewing any of its sessions marks the whole slug seen, even if a different
named session produced the fresher unreviewed output.

There's no separate "unread" flag — it *is* the `review-output` bucket
(rendered as a dim-warn `●`). A task lands there when its freshest session
tail ended in `end_turn` or `paused` more recently than the last time you
looked at that slug's session. "Looked at" is tracked by
`core/task-focus.ts`'s `taskFocusStore`: every time the hub retargets the
right pane at a slug's harness session, it stamps `now` for that slug into
`~/.cache/wt/task-focus.json`. That stamp is what `turnEndedAt >
lastFocusedAt` compares against — push-based, no polling, and persisted so a
`wt` restart doesn't re-flag everything as unread. The line-2 detail text for
a `review-output` task is the agent's last written line
(`SessionTail.lastAssistantText`); for a `needs-you`/"agent is asking" task
it's the pending question (`SessionTail.pendingAsk`) — both come from
`core/harness/claude/jsonl.ts`'s tail parser.

A displayed session also counts as seen: while a task's harness session
is showing in the right pane, wt re-stamps its focus clock whenever new
output lands, so a turn that finishes in front of you never files itself
under Review output.

## Keymap (hub-only)

| key | action |
|---|---|
| `⌘J` / `⌘K` | move the task cursor (the right pane live-follows) |
| `⌘1`-`⌘9` | jump straight to task N (dim ordinals on the first nine rows) |
| `⌘L` | start (or resume) and show the task's agent session; on a PR task, `Enter` opens the PR; repeat = toggle pane focus |
| `⌘D` / `⌘S` | show the diff / shell session; repeat = toggle pane focus |
| `⌘H` / `Esc` | focus the task pane / bounce back to the session |
| `⌘E` | expand/collapse the selected stack or the Sessions group |
| `⌘Z` / `⌘P` | snooze until the bucket changes / pin to top |
| `⌘I` | toggle the stacked details card |
| `⌘O` / `⌘M` | open the PR / toggle auto-merge |
| `⌘T` | new-worktree prompt |
| `⌘.` / `⌘;` | action picker / sessions picker |
| `⌘W` | close the task's session gracefully (`⌘W` ×2 to confirm) |
| `⌘⌫` | remove the worktree (confirm) |
| `⌘F` | zoom the session pane full-width |
| `⌘R` / `⌘/` | refresh / help |
| `q` (task pane focused) / `Ctrl+C` | leave the hub — kills the outer layout only; inner sessions keep running |
| `Enter`/`Tab`/`j`/`k`/`g`/`G`/`z`/`P`/`I`, `F7`-`F12` | direct equivalents when the task pane holds focus (F7 focus-tasks, F8 zoom, F9 focus-toggle, F10-F12 shell/diff/agent) |

Moving the task cursor auto-follows: after a 150ms debounce, the right pane
switches to the newly-selected task's live session (stamping its focus
clock) or falls back to the home dashboard if it has none.

## Limitations

- **Remote worktrees don't appear.** The `[remote]` SSH-host inventory is a
  separate query from the local worktree rows the task pipeline consumes
  (`useTaskRows` never sees it), so remote rows are simply absent from the
  inbox — switch to classic mode (`wt classic`) to see or act on them.
- **Manual sections and row ordering don't apply.** The inbox always sorts
  by pin → bucket → snooze → recency; there's no equivalent of classic
  mode's section picker (`l`) or manual reordering (`J`/`K`) changing where
  a task lands here (those keys still work and still affect classic-mode
  layout, they just don't move anything in the inbox).
- **Archived worktrees are hidden**, unlike classic mode's list, which can
  still show them via the archived section.
- **The removed-worktrees history view is hub-less** (`h` is classic-only;
  `⌘H` means focus-the-task-pane in hub) — use `wt classic` to browse or
  restore removed worktrees.
- Classic and hub mode are two views over identical state — nothing needs
  reconciling when you switch, so treat `wt hub` / `wt classic` as a
  free toggle rather than a mode commitment.

## The Sessions group

The classic `,` / `.` / `/` slot keybindings do not exist in hub mode.
Instead the inbox ends with a **Sessions** group: a bottom-pinned entry for
the main clone's harness session, Tab-expandable to the wt-source and
dotfiles slots. Slot entries behave like any task — selecting one
live-follows its session, ⏎/F12 starts + shows it (and toggles pane focus
on a repeat press) — so the right pane always corresponds to the selected
entry and there is no special "viewing a slot" state to track. The hub
also drops the classic bottom bar entirely (its robots moved into the
Sessions entries); the footer only appears transiently for prompts and
toasts.

