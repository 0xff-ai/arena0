# D3b Program scenario tests run on real Hosts; delete the native SDK harness

## Contract

Owner decision 2026-09-25. The shapes below are decided. Implement them exactly.
Add no type, function, trait impl, module, feature or re-export that is not
listed. Private local variables are fine.

**Escalate instead of improvising.** If a shape cannot be achieved as written
(it does not compile, it loses coverage this contract does not let you drop, or
it needs an Arena method not listed here), stop that item. Put in your report
the item, the exact obstacle (file:line, error or test) and the options you
see. Items that do not depend on the blocked one may be finished first. Do not
add an Arena method yourself.

### 1. Deletions

- `crates/arena0-sdk/src/testing.rs`, the whole `crates/arena0-sdk/src/testing/`
  directory, and `crates/arena0-sdk/tests/fixtures.rs`.
- `crates/arena0-sdk-macros/src/arena0_test.rs`, its `mod arena0_test;` line and
  the `#[proc_macro_attribute] pub fn test` that calls `arena0_test::expand` in
  `crates/arena0-sdk-macros/src/lib.rs`.
- In `crates/arena0-sdk/src/lib.rs` and `crates/arena0-sdk/src/prelude.rs`:
  `pub mod testing`, the `test` macro re-export, every re-export and doc link
  that names `testing`, `DivergenceDiagnostic` or `DivergenceKind`.
- `crates/arena0-protocol/src/trace/divergence.rs` (`DivergenceDiagnostic`,
  `DivergenceKind`, `JsonDiffExt`), its `mod` line and the `pub use` in
  `crates/arena0-protocol/src/trace/mod.rs`, and the names in the
  `crates/arena0-protocol/src/lib.rs` re-export. Nothing outside the deleted
  harness uses them (checked).
- Cargo dependencies, dev-dependencies and features that only the deleted code
  used, in `crates/arena0-sdk/Cargo.toml`, `crates/arena0-sdk-macros/Cargo.toml`
  and `crates/arena0-protocol/Cargo.toml`. Do not change any version.
  `Cargo.lock` changes only by removal.

### 2. Native host shims: `crates/arena0-sdk/src/effects.rs`

Every non-`wasm32` branch of a host shim (`host_log`, `host_random`,
`host_broadcast`, `host_set_timer_spec`, `host_guest_sign`, `host_end_session`,
`host_abort_session`, `host_fail`, and the state read/write/len shims) becomes a
call to this one private function, added to `effects.rs`:

```rust
/// Host imports exist only inside the Wasm guest. Native builds compile the
/// SDK for pure program tests, which never reach a host import.
#[cfg(not(target_arch = "wasm32"))]
#[cold]
fn native_host_unavailable(import: &'static str) -> ! {
    panic!("arena0 host import `{import}` is only available inside the Wasm guest")
}
```

- The `import` argument is the matching `imports::*` constant.
- Arguments that are then unused are consumed with one `let _ = (a, b);` line
  directly before the call. Write no other code in those branches.
- Delete `fake_sign` and every native effect sink/buffer that only fed the
  harness.
- Delete the SDK unit tests in `crates/arena0-sdk/src/context.rs` that read
  `crate::testing::drain_effects()` or call `ctx.sign(...)` natively. Map each
  to "dropped: native effect sink only". Keep the three `compile_fail`
  doctests on `EffectMode` and every other `context.rs` test.
- If any remaining native test then panics in `native_host_unavailable`, it
  drove a handler natively: move it to an Arena test (section 4) or map it to
  a section-5 drop reason. Do not restore a native sink.

### 3. Arena API additions: `crates/arena0-tests/src/arena.rs`

Exactly these public items:

```rust
/// One open callout on a participant, as the Host reported it.
#[derive(Debug, Clone, PartialEq)]
pub struct ObservedCallout {
    pub callout_index: u32,
    pub context: serde_json::Value,
}

impl Run {
    /// Wait until `participant` has an open callout and return it without
    /// answering it. A later `expect_input(participant)` answers the same callout.
    pub async fn callout(&mut self, participant: usize) -> ObservedCallout;

    /// Run one read-only query on `participant` through its execution actor
    /// (`ExecCommand::Query`) and return the response JSON.
    pub async fn query(
        &mut self,
        participant: usize,
        query_index: u32,
        query: serde_json::Value,
    ) -> serde_json::Value;

    /// Render one view on `participant` through its execution actor
    /// (`ExecCommand::View`).
    pub async fn view(&mut self, participant: usize, viewport: Viewport) -> View;
}

impl Expect<'_> {
    /// Answer the open callout with bytes the program must reject. Return the
    /// program's rejection reason, or panic if the answer was accepted. The
    /// callout stays open, so a later `respond_bytes` answers it.
    pub async fn respond_rejected(self, data: Vec<u8>) -> String;
}
```

`Viewport` and `View` are `arena0_protocol::{Viewport, View}`.

Bodies:

- `callout` uses the same deadline, `drain_events`, termination panic and
  timeout panic as `respond_bytes`. It finds the newest
  `SessionMessage::CalloutRequested { callout_index, context, .. }` in the
  participant's events without removing it and parses `context` with
  `serde_json::from_slice(&context).expect("callout context is JSON")`.
- Move the wait-and-submit loop of `respond_bytes` into this one private method
  on `Expect`, which does not remove the `CalloutRequested` event:

  ```rust
  async fn submit(&mut self, data: Vec<u8>) -> Result<(), ExecError>;
  ```

  `respond_bytes` becomes: `submit`, panic with the participant index on
  `Err`, then remove the `CalloutRequested` event (the current `retain`).
  `respond_rejected` becomes: `submit`; on
  `Err(ExecError::InputRejected(reason))` return `reason`; on `Ok(())` or
  any other error, panic with the participant index. This is the only new
  private helper.
- `query` sends `ExecCommand::Query { query_index, query: JsonBytes::try_new(serde_json::to_vec(&query).expect(..)).expect(..), reply }`,
  awaits the reply, panics with the participant index on any error, and parses
  the returned `JsonBytes` into `serde_json::Value`.
- `view` sends `ExecCommand::View { viewport: <viewport serialized with serde_json into JsonBytes>, reply }`,
  awaits, panics with the participant index on error, and returns the `View`,
  dropping the version.
- `Arena::params` already exists for custom params. Add nothing else.

### 4. Where moved tests go

- Keep native `#[test]`s in `programs/*` and `examples/minimal-program` that
  call pure program functions and types (rules, scoring, legality, allocation,
  codecs, `Program::outcome` / `ProgramView::view` / `writer` on a constructed
  `Shared`, schema metadata). They must not construct a `Context`,
  `LocalContext` or `CalloutContext` and must not call a handler. The existing
  pattern to follow is
  `programs/vickrey-auction/src/lib.rs::view_fills_all_slots_and_mono_has_no_escape_sequences`.
- Every test in `programs/*` that drove handlers (`TestHarness`, `Harness`,
  `Scenario`, `BilateralPair`, `#[arena0::test]`) is either rewritten into that
  program's existing Arena file or dropped with a section-5 reason:

  | program | Arena file |
  |---|---|
  | chess | `crates/arena0-tests/tests/chess_bilateral.rs` |
  | contract-net | `crates/arena0-tests/tests/contract_net_multiparty.rs` |
  | cumulative-sum | `crates/arena0-tests/tests/cumulative_sum_trilateral.rs` |
  | prisoner-dilemma | `crates/arena0-tests/tests/prisoner_dilemma_bilateral.rs` |
  | rock-paper-scissors | `crates/arena0-tests/tests/rock_paper_scissors_bilateral.rs` |
  | sequential-count | `crates/arena0-tests/tests/sequential_count_multiparty.rs` |
  | vickrey-auction | `crates/arena0-tests/tests/vickrey_auction_multiparty.rs` |

  Create no new Arena test file.
- `examples/minimal-program` is built against the packaged SDK, not by
  `just build-programs`, so it gets no Arena test. Its two tests become these
  native pure tests in its `mod tests`, and the `arena0::testing` import goes:

  ```rust
  #[test]
  fn outcome_ranks_choices_by_score() {
      // Shared { phase: Phase::Choosing, choices: [Some(Choice::One), Some(Choice::Two)] }
      //   -> Outcome::Win { winner: Participant::new(1), choices: [Choice::One, Choice::Two] }
      // Shared { phase: Phase::Choosing, choices: [Some(Choice::Two), Some(Choice::Two)] }
      //   -> Outcome::Draw { choices: [Choice::Two, Choice::Two] }
      // via <minimal_choice::MinimalChoice as Program>::outcome(&state)
  }

  #[test]
  fn view_uses_all_four_slots_and_plain_text() {
      // state = Shared { phase: Phase::Choosing, choices: [Some(Choice::One), None] }
      // ensemble = Ensemble::from_peers(vec![PeerId([1; 32]), PeerId([2; 32])]).expect("valid view ensemble")
      // view = <minimal_choice::MinimalChoice as ProgramView>::view(&state, &ensemble,
      //            &Viewport { width: 80, color: ColorDepth::Mono })
      // assert: all four slots present; no "\x1b[" in any slot;
      //         Agents slot contains "P0: one" and "P1: waiting";
      //         StatusBar slot == "choosing"
  }
  ```

  These replace `two_replicas_converge_on_the_choices` and the
  `#[arena0::test]` view test.
- One Arena run per distinct setup. A run checks, in order: callout contexts
  (`callout`), rejected answers (`respond_rejected`, asserting the reason text
  the program returns), views/queries mid-game, then the outcome and receipts.
  Add a run to an existing file only for a setup that differs from every
  existing run in that file (params, participant count, or a different path
  through the game). Otherwise extend an existing run.
- Assert observable results only: callout index and context fields, rejection
  reasons, `View` slots, query JSON, outcomes, receipt verification. Never
  decode shared-state bytes in a test.
- Where a native pure test already proves a rule (for example the payoff
  matrix), do not re-prove it on a Host.

### 5. Allowed drop reasons (use one verbatim)

- "dropped: native harness only". The test asserted harness mechanics (replay
  reports, coverage, pending ledger, delivery schedules, fault status,
  divergence diagnostics).
- "dropped: native effect sink only". The test read effects or signatures
  that only the native sink produced.
- "dropped: internal state". The test asserted shared-state fields with no
  observable projection. Allowed only if another kept test asserts the same
  rule through a callout, view, query or outcome. Name that test.

### 6. Docs

- Rewrite the program-testing sections of `docs/programming.md` and
  `docs/development.md` to say: pure logic gets native unit tests in the
  program crate; behavior gets Arena tests in `crates/arena0-tests`, which need
  `just build-programs` first.
- Grep `docs/`, `README.md`, `examples/`, `crates/arena0-sdk/src` (crate docs)
  and `crates/cargo-arena0` for `testing::`, `TestHarness`, `Scenario`,
  `arena0::test` and `BilateralPair`. Remove or rewrite every hit.
  `cargo-arena0` generates no test file today (checked); change nothing there
  unless the grep hits.
- Routing needs no code change: `justfile` `test-programs` already runs all of
  `arena0-tests` for any `programs/*` change. Do not edit `justfile` or
  `scripts/check-affected.sh`.

## Acceptance

Run `cargo fmt --all`, `just build-programs`, `just check` and `just test`. All
must pass. Time `just test` and `cargo nextest run -p arena0-tests` with `time`.
Daemon MCP tests in `arena0-tests` (`daemon_api`, `daemon_e2e`) are known to
time out under heavy machine load, on the base commit too. If only those fail,
rerun just the failing tests once and report both results. Do not change their
timeouts.

`rg -n "testing::|TestHarness|BilateralPair|arena0::test\b|DivergenceKind|DivergenceDiagnostic|JsonDiffExt|fake_sign" crates programs examples docs README.md`
must print nothing (ignoring `target/`).

Your report must include:

- one line per section above;
- deleted lines (production / test), from `git diff --stat`;
- the coverage table: every deleted or moved test mapped to
  "native pure test <path>::<name>", "Arena test <file>::<name>" or one
  section-5 drop reason, verbatim;
- the wall times;
- any deviation (there should be none);
- the gate tails.
