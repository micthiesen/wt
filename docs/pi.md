# Pi sessions

wt supports Pi 1.2.0 (`@earendil-works/pi-coding-agent`) as a native interactive
harness. Select Pi with `Shift+F12`, then `p`, or choose a Pi conversation in
the `;` session picker. Set `[harness] primary = "pi"` to make it the default;
omit Pi from `harness.hidden` to keep it selectable with another default.

Each target has one live `<slug>-pi` tmux slot. Conversations retain Pi's exact
session IDs and native display names. Resuming a conversation checks its checkout
identity; a slot without a proven conversation ID remains unknown instead of
borrowing another conversation's status. Pi's own `/new` and `/resume` update the
slot's recorded identity. Closing wt leaves the interactive session running.

wt embeds a small Pi extension and installs it at a content-addressed path. The
extension exposes a private host-local Unix socket for status, messages and
compaction. It runs inside the native Pi TUI; no separate RPC process owns the
conversation. Message delivery uses Pi's follow-up queue and expands native skill
commands such as `/skill:start`. Compaction uses Pi's compaction API and reports
completion or failure; it never sends `/compact` as inert chat text.

Requests carry the exact checkout and session ID. A receipt is recorded before
a message is submitted. Lost replies or uncertain outcomes are not automatically
resent. The bounded receipt journal belongs to the exact conversation and fails
closed when full rather than forgetting old requests. Restarting or resuming that
conversation retains its receipts; a new conversation gets its own journal.

Live glyphs and colors come from Pi's running state, queue and context usage.
Recent output comes from bounded reads of that exact conversation's transcript.
wt does not invent subscription quota estimates for Pi providers.

## Configuration and managed hosts

Pi keeps its native settings, extensions and provider authentication. wt does not
copy Codex refresh tokens or replace Pi's configured providers. Session discovery
uses `PI_CODING_AGENT_SESSION_DIR`, then `PI_CODING_AGENT_DIR/sessions`, then
`~/.pi/agent/sessions`. Skills use Pi's native `/skill:` syntax.

When Pi is installed and visible on the controller, managed provisioning installs
the compatible package in the remote managed root and activates it only after
validating its package identity and version. The embedded control extension comes
from the matching wt worker. The remote host retains its own Pi settings and
credentials; sign in to an authenticated provider on that host when needed.
Unsupported runtimes fail explicitly before a session starts.

Pi as the primary harness also supports prompt actions, manager messaging and
automation; interactive sessions preserve the user's configured extensions and
permission policy.

## Verification

Rust adapter tests cover identity, discovery, subprocess construction and socket
delivery. `bun test crates/wt-harness/src/pi/extension.test.ts` exercises the
control protocol (it needs Bun on the developer's machine and is not part of CI). `python3 crates/wt-harness/src/pi/native_smoke.py` exercises the
installed native TUI against a loopback mock provider, with isolated configuration
and sessions and no paid requests or user credentials. The Linux and macOS native
CI jobs also run `scripts/native-pi-check.py` against the built wt binary, covering
startup skills, discovery, compaction, F12, the session picker and scoped removal.
