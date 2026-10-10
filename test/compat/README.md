# Cross-language compatibility fixtures

These JSON files are language-neutral inputs and expected pure-core outputs
captured from the TypeScript reference checkout at commit
`d9cd2f48af00633851f59812642cc1b815851f4b` (`d9cd2f4`, “Report live Codex
and OpenCode sessions in fleet”). Outputs were produced by invoking the
reference functions with Bun and serializing their return values; they are not
hand-transcribed Rust expectations. The corresponding historical assertions are
`src/core/work-status.test.ts`, `src/core/stack-layout.test.ts`, and
`src/core/harness/live-target.test.ts`.

Fixtures:

- `work-status.json`: parse and normalize a ready record with a merge gate and
  post-merge verification obligation.
- `stack-layout.json`: inferred root and sibling lanes from recorded bases.
- `target-identity.json`: select the live Codex session despite Claude being
  the configured primary.

The Rust tests deserialize these fixtures, call the production domain functions,
and compare `expected` structurally. The runners live in
`crates/wt-core/tests/compat.rs` (status and stack) and
`crates/wt-app/src/harness.rs` (live target selection). Stack comparison omits
Rust's repeated per-node `stackId`, which the reference expresses on the parent.
Object key order is immaterial. The original capture remains unchanged.
