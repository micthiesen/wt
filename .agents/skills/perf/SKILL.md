---
name: perf
description: Investigates machine freezes, wt input latency, high CPU or memory, and runaway processes using measured load and known failure signatures. Use for performance diagnosis or fixes; maintain the adjacent notes.md with evidence and unresolved causes.
---

# perf — investigate & improve performance

You are the perf debugger for this machine and this codebase. The
deliverable is an **assessment plus a proposed fix** — do not apply
code changes or kill processes without an explicit go-ahead, unless the
user's invocation already asked for a fix.

**Read [notes.md](notes.md) FIRST.** It holds the current
baseline, open issues, and the learnings ledger — a symptom you're
about to investigate may already have a known signature there.

## 1. Capture the right workload

- For machine pressure, run `wt perf --json`. Its `%CPU` is the process's
  lifetime decaying average, not an instantaneous profile. For current load,
  compare `top -l 2 -n 12 -o cpu -stats pid,cpu,mem,command` samples.
- For native TUI latency, inspect `input-latency` records in the selected
  configuration's log directory (`wt-native.YYYY-MM-DD.log`). These measure
  input receipt through terminal draw completion; no restart flag is needed.
  They exclude terminal transport before input receipt and physical display
  latency. The renderer is `crates/wt-tui/src/terminal.rs`.
- Use `scripts/native-ui-check.py --binary target/debug/wt --sections
  --output /tmp/<unique-fixture>` for an isolated PTY check with delayed Git,
  external edits, metadata-only writes, and shutdown. Its key-to-output sample
  supplements the internal draw metric; neither is a physical paint probe.
- Use `scripts/perf-baseline.py --help` for repeatable idle, navigation and
  refresh measurements. Compare equivalent fixtures and optimized binaries.
  Include children in CPU accounting; report cold startup separately from
  steady-state work. Never label a reduced-feature workload representative
  without naming the disabled sources.

## 2. Diagnose

- Separate wt's CPU from harnesses, databases, browsers and OS processes.
  Correlate cwd and session identity for shared daemons outside wt ancestry.
- RSS is not macOS physical footprint. Memory in use is active, wired and
  compressed memory, not a subtraction from `os.freemem()`.
- Native source scheduling lives in `wt-runtime` and `wt-app` source modules.
  Check invalidation counts, last-good snapshots, bounded queues, watcher
  registration and cancellation before increasing poll intervals or timeouts.
- Keep filesystem/process work off the input path. An idle terminal should
  not continuously redraw. Preserve batching and start-time rate floors.
- Use isolated fixtures for fault injection. Do not stop live user sessions
  merely to obtain a cleaner benchmark.

## 3. Attribute honestly

The report is framed as **wt-downstream vs the rest of the machine**,
and the "Heaviest processes NOT downstream of wt" block is the point:
when the hog is a browser or another app, say so plainly and stop —
don't hunt for a wt/agent explanation to justify the invocation.
Compare wt's share against CPU *in use*, not installed capacity.

## 4. Report

Report: what the load is, whose it is, whether it's reasonable, and if
not, the specific fix (code change, kill line, config change) — with
the evidence. Wait for the go-ahead before mutating anything. A
ready-to-run `kill` line for leaked instances is a proposal, not an
action.

## 5. Update notes.md — every invocation

`notes.md` is a living document; keeping it current is part of the
skill, not optional:

- Refresh the **Baseline** section when you capture a snapshot that's
  representative (or when hardware/workload shifts make the old one
  stale). Keep it a summary, not a paste of the full report.
- Add/resolve entries in **Open issues** as they're found/fixed
  (resolved ones move to the ledger with the fixing commit).
- Append to the **Learnings ledger** whenever an investigation produces
  a reusable signature, technique, or trap — the test is "would the
  next investigation go faster knowing this".
- Prune: stale baselines and superseded learnings get rewritten, not
  accumulated.

Commit `notes.md` with the investigation under the current task’s branch and
shipping authorization.
