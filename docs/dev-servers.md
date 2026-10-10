# Development servers

`wt dev` keeps one supervised development server per local worktree. The
process runs in a private tmux session named `<slug>-dev`, so it outlives the
agent terminal while remaining tied to the worktree. Each slug keeps its
allocated port in wtstate. `{{port}}`, `{{slug}}`, and `{{path}}` are available
to configured command hooks.

`wt dev start [slug]` starts a server, or joins an existing startup. A server
that is already serving is restarted; a crashed server is recycled. It reports
that the process launched, not that the project is ready. Add `--wait` to wait
for a free slot and then for a TCP connection. One `--timeout` budget covers
both waits. A full slot cap exits with status 75. `--rebuild` runs reset
semantics before startup. `wt dev reset [slug]` stops the server, runs
`reset_command`, and starts again. If `stop_command` fails, reset stops before
running `reset_command`, because external state may still be active.

`wt dev status [slug]` reports running, starting, crashed, port, URL, queue
position, restart attempts, and whether the recorded start commit is no longer
an ancestor of the worktree head. For a running server this explicit command
also runs the configured health check once. `status --all` reports all local
worktrees, the slot holders, and the queue. Background status snapshots do not
run health commands. `wt dev logs [slug]` shows the recent supervisor output,
including a bounded retained crash summary after a terminal failure.

When `[dev_server].max_concurrent` is set, capacity is derived from live
`<slug>-dev` tmux sessions rather than a separate slot ledger. `start --wait`
joins a durable queue entry carrying the waiting process id and age. Readers
discard entries whose process has exited. Only a human shell may prioritize an
existing waiter with `wt dev queue <slug> --first`; `--normal` returns it to
FIFO order. Worktree agents cannot promote themselves.

The native supervisor restarts short-lived failures with bounded exponential
backoff and parks repeated deterministic failures. Intentional stop signals do
not restart the server. Restart counts, last exit code, marker time, and the
last useful crash output remain available to status and logs. Health checks
are never polled by the background snapshot path.

The isolated end-to-end check uses a temporary Git repository, private HOME,
private tmux socket, and local HTTP server:

```sh
WT_BUILD_ID=<full-build-id> cargo build -p wt-app -p wt-launcher --locked
python3 scripts/native-dev-check.py --binary target/debug/wt
```
