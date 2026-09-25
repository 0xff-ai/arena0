# Stage D milestone review

## Contract

You are a delegated agent performing a READ-ONLY major milestone review. You are not alone in the codebase. Do not modify source, revert others' edits, commit, push, spawn agents, or rerun full suites. Parent owns API design, disposition, fixes, and integration.

Review `/data/arena0` at checkpoint `ae94a88`, whole branch against base `5caeea9e4a4fb4ea6e9edbfae72e82ded881c891` (local main). This is a milestone review, not another D3b-only review. Existing implementer thr_k5qbnvnywi is recorded only as provenance.

```rust
// REVIEW CONTRACT: no implementation changes are authorized.
// Preserve one authoritative owner per durable fact.
// SDK: Ctx<Shared, Local, M>; modes preserve agreed/local/read capabilities.
// Sandbox: plain-argument calls; required exports owned by REQUIRED_FUNC_EXPORTS.
// Serialization: bounded decoding; derived Borsh encodings remain byte-compatible.
// Engine: compile-once sharing and stable persistent Wasmtime cache reuse.
// D3b: native pure tests + real-Host Arena behavior tests.
// Final accepted D3b surface:
// Run::callout(&mut self, participant: usize) -> ObservedCallout
// Run::view(&mut self, participant: usize, viewport: Viewport) -> View
// Expect::respond_rejected(self, data: Vec<u8>) -> String
// Run::query was deliberately removed by the parent: no real caller.
```

Read `.bb/handoff/claude-handoff-ledger.md` for provenance, latest parent rulings and evidence. Original branch audit and task ledger live in `/home/ubuntu/.bb-machines/raul.getbb.app/thread-storage/thr_6b9is45iux/impl/`. D3b contract lives in `/home/ubuntu/.bb-machines/raul.getbb.app/thread-storage/thr_k94vjvhz6a/D3b.md`; final parent amendments in the ledger supersede it. `docs/protocol-architecture.md` owns behavior; report disagreements rather than choosing a new behavior.

Focus review on concrete correctness and lost promises:

1. Whole-branch public/internal API ownership, duplicate concepts, unnecessary abstractions, protocol distinctions and caller migration. D2b has not had independent model review.
2. Bounded codec limits and derived tags/golden encodings, validated decode boundaries, durable fact ownership after store changes, SDK mode constraints.
3. Compile-once and stable cross-run Wasmtime cache requirement: actual production and Arena paths, not merely available helpers.
4. D3b deleted-test coverage: original tests versus retained pure tests / real-Host assertions. Parent restored generic data-macro bounds and chess draw-rule tests. Identify uncovered product-rule, rendering, rejection, or security assertions; do not treat all deleted harness users as harness-only promises.
5. Terminal views: execution actor is reported to stop before a final view can be requested. Determine whether a lost pure rendering test can be retained independently, whether the real public path already supports stored terminal projections, and whether docs/architecture promise terminal viewing. Distinguish existing limitations from introduced regressions. Do not prescribe a new runtime without evidence.
6. Arena callout bookkeeping/rejection retention, race/deadline behavior, and new scenario assertions. Existing successful runs do not prove race freedom.
7. Recorded follow-ups: bounded helpers in wire versus program; public but internal-only resolve_test_cache_dir. Recommend owner/visibility using actual dependency evidence; do not implement.

## Acceptance

Return severity-ranked actionable findings with exact file/line, trigger, mechanism, observable consequence, and bounded remedy. Distinguish introduced bugs, pre-existing issues, acceptable parent deviations, and uncertain concerns. Review actual code and tests; do not count test totals as coverage proof. State what you inspected and any limits. If no findings, say so explicitly.

Reuse existing verification: `.bb/d3b-gate.log` records fmt/build-programs/check/test exit=0, 727 host tests and 22 program tests after parent fixes. Do not rerun expensive tests unchanged. A focused read-only diagnostic is allowed when it resolves a specific uncertainty. Preserve all source and compiler cache configuration.

Write the full review to `/home/ubuntu/.bb-machines/raul.getbb.app/thread-storage/thr_jsrdc8mjrq/stage-d-milestone-review.md`; report its path and concise findings in the final response. This report is your only write lane.
