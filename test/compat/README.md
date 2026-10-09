# Cross-language compatibility fixtures

These JSON files are language-neutral inputs and expected pure-core outputs
captured from the TypeScript reference checkout at commit
`d9cd2f48af00633851f59812642cc1b815851f4b` (`d9cd2f4`, “Report live Codex
and OpenCode sessions in fleet”). Outputs were produced by invoking the
reference functions with Bun and serializing their return values; they are not
hand-transcribed Rust expectations. The closest existing assertions are
`src/core/work-status.test.ts`, `src/core/stack-layout.test.ts`, and
`src/core/harness/live-target.test.ts`.

Fixtures:

- `work-status.json`: parse and normalize a ready record with a merge gate and
  post-merge verification obligation.
- `stack-layout.json`: inferred root and sibling lanes from recorded bases.
- `target-identity.json`: select the live Codex session despite Claude being
  the configured primary.

The Rust side should deserialize each fixture, call the matching pure domain
function and compare `expected` structurally. Do not compare JSON object key
order. Add edge cases from the source tests as new fixture cases when a Rust
port reaches that domain. No Rust implementation or fixture runner exists yet;
these baselines do not claim Rust coverage.
