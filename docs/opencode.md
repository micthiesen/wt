# OpenCode sessions

wt supports OpenCode V2. V1's session schema and CLI flags are not supported.
Install `opencode` on each host that runs sessions, configure its native provider
credentials there, and remove `opencode` from `harness.hidden`. Select it with
`Shift+F12`, or set `[harness] primary = "opencode"`.

Each target has one `<slug>-opencode` tmux slot. Before opening a new terminal,
wt creates a native V2 session through the OpenCode API and pins its exact ID
to the slot. Resume validates that the selected session belongs to the checkout,
is not archived, and is not a child conversation. Closing wt leaves the native
terminal and OpenCode service running. The `;` picker discovers V2 conversations
from the native database; it does not borrow another conversation's identity.

Before launching a cold terminal, wt waits for the native location's agent and
skill services to initialize. This prevents the TUI's health recovery from
restarting the service during slow watcher setup and interrupting the first
prompt. Existing live terminals skip these readiness reads. Invalid resume IDs
are rejected before starting native services.

Interactive terminals, API calls and generated headless actions share
the native service registration under `$XDG_STATE_HOME/opencode/service.json`
(normally `~/.local/state/opencode/service.json`). They pin an authenticated
`--server` connection so polling a busy service cannot restart it. Only missing
registration starts a service automatically, serialized across wt clients.
Malformed registration and a recorded process that has exited fail explicitly;
run `opencode service start` to recover an exited service, then retry. wt never
restarts a live service because a health request timed out or replays an
uncertain mutation. API passwords pass through child environment; interactive
launches consume a private one-use auth source file before executing OpenCode.

Messages use V2's durable prompt queue, not simulated terminal typing. Lost
admission replies are checked against the exact generated message ID; wt never
blindly repeats an uncertain send. Native compaction uses `session.compact`.
OpenCode does not accept custom compaction instructions, so the extra prose in
wt's shared compact action is omitted, as it is for Codex. Acceptance means the
request is queued; the session's output and status show subsequent progress.

Live status and output use the exact V2 session, messages and parts. Child
conversations are excluded from the top-level session picker. Provider costs
are costs, not subscription-quota estimates.

Permission requests and question forms show `asking` for their exact live
session in both the list and session pane. wt checks these native endpoints
every five seconds only while OpenCode slots are live. The endpoints are scoped
by checkout: shared locations are coalesced, at most two locations are checked
concurrently, and the whole batch has a two-second budget. A failed read shows
`unknown` for those OpenCode identities without hiding other harnesses. Clearing
the request restores the session's observed working or idle state.

## Skills and background actions

OpenCode V2 uses `@start`, `@manager` and other `@skill-id` references. `wt skills
sync` writes shared `~/.agents/skills` when that layout exists, otherwise native
`~/.config/opencode/skills`. Existing `.claude/skills` remain discoverable for
compatibility. wt does not require a Claude installation for OpenCode skills.

Prompt actions run `opencode run --auto` with the action's working directory.
This approves pending tool permissions while preserving explicit deny rules;
V2 otherwise rejects pending requests in headless mode and skips that work.
The removed V1 `--pure`, `--dir` and `--variant` options are never sent.
Provider authentication remains OpenCode's own configuration on the host.

The adapter is validated against OpenCode 2.0.29. Tests isolate databases and
tmux servers.
