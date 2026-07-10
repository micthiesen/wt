# GitHub webhooks

Without this, PR / checks / merge-queue badges stay fresh via a local `.git/refs` watcher, a 3-minute `git fetch origin` backstop, and manual `r`. That works, but check churn on the GitHub side (CI finishing, reviews landing) only shows up when one of those fires.

Add a `[github.events]` section and run the small local daemon to have GitHub **push** updates instead: badges flip within a second or two of the event, far fewer `gh` calls, and the daemon keeps a warm snapshot so a freshly opened TUI already shows current state. Config keys: [configuration.md](configuration.md#githubevents--optional-webhook-daemon).

It's a plain repo webhook — no GitHub App, no OAuth.

## Setup

```sh
wt events install     # writes a launchd agent + generates the HMAC secret
wt events start       # load the daemon
wt events status      # liveness, last delivery, snapshot age
```

`install` prints exactly what to paste into the repo's **Settings → Webhooks**: the payload URL, content type `application/json`, the generated secret, and the event checklist (`pull_request`, `pull_request_review`, `pull_request_review_thread`, `check_suite`, `check_run`, `status`, `merge_group`, `push`).

The daemon listens on `[github.events].host` (default loopback); map a public HTTPS URL to it however you route traffic into your network — a tunnel or reverse proxy on the same machine forwarding to localhost is the simple case. If a reverse proxy on a *different* host has to reach this machine, set `host` to a LAN IP or `0.0.0.0`; the HMAC secret is then the only auth boundary, so keep the listener on a trusted network.

## Security model

- Every delivery is verified against `X-Hub-Signature-256` (HMAC, constant-time compare). Unsigned or mis-signed requests are rejected.
- Webhook payloads are a **refresh signal, never a data source**: the daemon only ever re-runs the same read-only `gh` fetch the TUI already uses. A forged payload's worst case is an extra fetch.
- `wt events secret` rotates or shows the secret; `wt events uninstall` removes the launchd agent.

Omit the `[github.events]` section entirely and nothing changes — the daemon subcommands just refuse to run, and the TUI stays in watcher + backstop mode. If the daemon dies mid-session, `backstop_poll_ms` (default 10 minutes) bounds how stale the badges can get.
