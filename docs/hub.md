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

## Terminal focus and the Alt key layer

Your terminal's keyboard focus normally sits on the right (session) pane —
that's where you're typing to Claude/Codex/etc. wt still needs a way to
receive keystrokes without you switching panes for every action, so the
outer server's root key table forwards `Alt+<key>` chords straight into the
left pane as the bare key (`core/hub/naming.ts`'s `HUB_FORWARD_KEYS`,
rendered as `bind -n M-<key> send-keys -t hub:0.0 <key>` in
`core/hub/config.ts`). Concretely: **every classic-mode single-key binding
works in hub mode as Alt+that-key** (`Alt+j`/`Alt+k` to move, `Alt+n` for a
new worktree, `Alt+d` to remove, `Alt+r` to refresh, `Alt+;` for the sessions
picker, and so on), plus `Alt+Enter` and `Alt+Tab`. Modifier-combos that
aren't plain single keys (`Ctrl+J`/`Ctrl+K` scroll, `Ctrl+N`/`Ctrl+R`/`Ctrl+A`,
`Shift+Tab`, `Shift+F10`/`F11`/`F12`) have no hub-level forwarding — reach for
classic mode if you need one of those.

Two bindings are exceptions with no Alt prefix, handled by tmux itself
instead of relayed to wt:

- **`F9`** cycles pane focus (left ↔ right). Forwarded to wt like
  F10-F12: wt runs the `select-pane` itself and stamps its focus
  indicator in the same stroke, so the signal can't drift.
- **`F8`** zooms the right pane to full-screen — `resize-pane -Z`.

Which pane holds focus is signaled **inside the task pane** (the session
pane is left unmarked): the title bar shows `⌨ tasks` (accent) when typing
lands in the task pane and `⌨ session` (dim) otherwise, and the tasks
panel border tints accent while focused. When the right pane is showing a
special slot session (`,` / `.` / `/`) instead of the selected task, the
title bar flags it with a warn-colored `◂ <label>` badge — and manually
refocusing the task pane (F9, mouse) snaps the right pane back to the
selected task's session.

`F10`/`F11`/`F12` are also forwarded un-prefixed (not `Alt+F10` etc.) since
they're already dedicated function keys distinct from ordinary letters.

**Prerequisite:** your terminal has to actually send `Alt+<key>` (or
`Option+<key>` on macOS) as a distinguishable Meta/CSI-u sequence for tmux's
`M-` bindings to fire at all — most modern terminals (iTerm2 with "Left/Right
Option Key" set to Esc+, WezTerm, Kitty, Ghostty) do this out of the box. The
hub's outer tmux config also turns on `extended-keys always` / `csi-u` so a
CSI-u-capable terminal gets unambiguous key events instead of the older
Esc-prefix heuristic.

When a modal picker or footer text prompt is open, wt temporarily pulls tmux
focus onto the left pane (so you can type into it directly) and hands focus
back to the right pane the moment it closes — you never have to reach for
`F9` just to answer a picker.

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
| `review-output` | a turn ended and you haven't looked since | the freshest session tail ended (`end_turn`/`paused`) more recently than your last recorded focus of that slug |
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

## The unread-output bit

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

## Keymap (hub-only)

Every classic-mode key still works (as `Alt+<key>`, see above); these are
additional or hub-specific:

| key | action |
|---|---|
| `Enter` / `F12` | start (or resume) and show the selected task's agent session in the right pane; on a PR task, opens the PR instead |
| `F11` / `F10` | show the task's diff / shell session in the right pane |
| `j`/`k`, `g`/`G` | move the task cursor |
| `Tab` | expand/collapse the selected stack task |
| `z` | snooze the task until its bucket changes |
| `P` | pin the task to the top |
| `D` | toggle the stacked details card below the task list |
| `,` / `.` / `/` | show the wt-repo / main-clone / dotfiles slot session in the right pane |
| `q` / `Ctrl+C` | leave the hub — kills the outer layout session only; every inner-server session keeps running |
| `F9` (no Alt) | cycle pane focus left ↔ right |
| `F8` (tmux-level) | zoom the right pane full-screen |

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
- Classic and hub mode are two views over identical state — nothing needs
  reconciling when you switch, so treat `wt hub` / `wt classic` as a
  free toggle rather than a mode commitment.
