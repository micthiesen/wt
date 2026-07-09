import { config } from "../../../core/config.ts";

export const HELP = `usage: wt stack <subcommand> [options]

subcommands:
  hunks [--holistic <b>] [--unified <n>] <file>...   list a file's holistic-diff
                             hunk ids (for hunk-level slice partitions; --json
                             for /split). --unified pins the diff context (the
                             stack's hunkContext, else 3); 0 splits coalesced edits
  apply <stackId>            materialize an already-ingested manifest
  apply --from <file>        strict-validate + ingest a manifest, then materialize
  plan --from <file>         strict-validate + ingest only (no materialize); prints stackId
  status [stackId]           render the manifest DAG + drift vs reality (defaults
                             to the current branch's stack; --all for every stack)
  context                    read-only pre-split context for /split (branch, base
                             decision, changed files, wt size); runs in the cwd
                             worktree, not the main clone
  section <stackId> <sliceIdOrPr> [label]   print one slice's static PR-body
                             "Stack" section (flat list, or a tree for a fork)
  split <stackId> <sliceId> --from <frag>   reshape: replace an open slice with N
                             sub-slices (re-threads descendants). Manifest only;
                             prints the apply/replay/retire next steps (or --apply
                             [--verify] to chain reshape → apply → replay)
  add [<branch>] [<stackId>] append an EXISTING branch to a live stack as a new
                             tip slice (adopts its open PR, or opens a draft PR);
                             never creates branches/worktrees — \`wt new\` does that
  reconcile [stackId]        manifest bookkeeping only: mark merged PRs, reparent
                             children (incl. a landed external/stack-on-stack parent)
  replay [stackId]           squash-safe replay each slice onto its parent (+ retarget PRs)
  rebase [stackId]           reconcile then replay (the one-shot /restack does)
                             (stackId defaults to the current branch's stack)
  prune-backups [--days N]   delete backup/restack-* + backup/stack-sync-*
                             branches older than N days (default 0 — all; the
                             commits stay recoverable via the reflog)

apply options:
  --from <file>              ingest a skill-authored manifest JSON (strict validation)
  --install                  run install per slice (default off — slices are install-free)
  --verify                   typecheck each cumulative slice prefix in a throwaway
                             worktree before opening any PR (needs [stack]
                             verify_command; aborts on the first red prefix)
split options:
  --from <file>              fragment JSON: array of { id, title, branch, files[] } sub-slices
  --plan                     preview the reshape without writing
  --apply                    chain reshape → apply → replay (still prints the PR-retire step)
  --verify                   with --apply: typecheck each new sub-slice prefix in a
                             throwaway worktree before opening any PR (needs [stack]
                             verify_command; aborts the chain on the first red prefix)
add options:
  (branch defaults to the current worktree's branch; a positional with a "/" is
   the branch, without is the stackId — resolved from --onto's branch when omitted)
  --onto <sliceId|branch>    parent to stack on (default: the fork base recorded
                             by \`wt new --base\` when it names a live slice, else
                             the highest-ordinal live slice; pass
                             ${config.branch.base} to root a new parallel lane)
  --title <t>                slice title (default: the PR's title, else derived
                             from the branch name)
status options:
  --json                     machine-readable output
  --all                      every stack manifest (default: the current branch's
                             stack, or all stacks when cwd is in no stack)
reconcile/replay/rebase options:
  --onto <ref>               trunk landed roots reparent onto (default ${config.branch.base})`;
