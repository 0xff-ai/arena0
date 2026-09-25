# D3b terminal projection coverage restoration

## Contract

You are a delegated agent. Implement only the test changes specified below. You are not alone in the codebase; preserve others' edits and adjust to concurrent changes. Do not spawn agents, commit, push, alter production visibility, add runtime hooks, or change dependencies. Parent owns API design and final acceptance. Escalate if a named shape cannot be achieved; do not invent helpers or widen the write lane.

Own only `programs/rock-paper-scissors/src/lib.rs`, `programs/chess/src/lib.rs`, and `programs/contract-net/src/lib.rs` in your worktree. Add only the following test-only symbols (imports inside the indicated test modules are allowed):

```rust
// programs/rock-paper-scissors/src/lib.rs, existing tests module
#[test]
fn terminal_view_renders_revealed_hands_and_scores() { /* specified below */ }

// programs/contract-net/src/lib.rs, existing tests module
#[test]
fn terminal_view_renders_accepted_assignment() { /* specified below */ }

// programs/chess/src/lib.rs, INSIDE pub mod chess so private Shared is accessible
#[cfg(test)]
mod projection_tests {
    #[test]
    fn terminal_view_and_outcome_preserve_chess_results() { /* specified below */ }
}
```

No new top-level helpers, production items, traits, fields, or re-exports. Use local variables/closures and existing primitive APIs for fixtures. Never construct a Context or call host imports. Call actual `ProgramView::view` and `Program::outcome`, not private render helpers or an implementation copied into the test. Pure data primitives can be driven through their real public methods to construct shared state. Existing vickrey `complete_protocol` test is the fixture pattern; inline the equivalent only where needed, do not change vickrey.

RPS: construct a completed commit/reveal for Rock vs Scissors, with scores [2,0], round/total-rounds suitable for a finished match. Drive CommitReveal through commit_with_salt/handle/take_reveal using fixed public test salts. Construct Shared through its root-visible fields and Default for ManagedPhase. Render with Mono and Ansi16 at width 120. Assert all four slots; State names P0 throwing rock and P1 throwing scissors; Agents contains their scores and revealed hands; Mono contains no escapes. Verify expected winner/scores through Program::outcome. Use stable text/glyph assertions from the old deleted test (`git show 92f2fb7:programs/rock-paper-scissors/src/lib.rs`) rather than assuming spelling.

Contract-net: one task and a real Agreement<AssignmentPlan> proposed to participants 0 and 1 with unanimous threshold, then both votes Accept. Assignment is P1 at cost 7. Construct Shared with this accepted agreement and meaningful worker offer/task data. Render Mono and Ansi16 at width 120. Assert all four slots, task name, `P1 @ 7`, and accepted status; Mono has no escape sequences. Assert Program::outcome returns the proposal ID and assigned plan. Do not invoke program handlers or alter ManagedPhase; rendering/outcome read agreement state.

Chess: a test module nested in `chess` may access Shared's private fields without widening visibility. Construct terminal Shared from real board positions: scholar's mate (derive board by legal move sequence using cozy_chess) and the three draw positions/moves already used by `draw_rules_classify_terminal_boards`. For each, compute actual status, set FEN/status and Default other fields, and call Chess's ProgramView/Program implementations. At width 120 in Mono, assert all four slots, both kings on the board, correct terminal status text, no escapes, and exact winner/draw reason in Outcome. The expected status/outcome must be specified constants per scenario, not obtained by calling the same projection as oracle. If macro handling disallows the nested test module, stop and report the actual compiler error plus bounded options; do not widen fields.

The milestone review overclaimed two other gaps: vickrey already has terminal sale assertions, and daemon_api::competing_callout_submissions_return_typed_conflict_and_execution_continues retries after completion and asserts CalloutNotPending. Do not duplicate or alter those tests.

## Acceptance

Run `cargo fmt --manifest-path programs/Cargo.toml --all --check` (format only owned files if needed), then `cargo test --manifest-path programs/Cargo.toml -p rock-paper-scissors -p chess -p contract-net`. Retain full output for failures, report command exit status and all added assertions. Do not run the aggregate workspace gate or rebuild Wasm for these cfg(test)-only changes; parent owns the final affected gate. Report actual diff and deviations, if any.
