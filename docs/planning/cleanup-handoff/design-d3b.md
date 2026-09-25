# D3b: program scenario tests run on real Hosts; delete the native SDK harness

Owner decision 2026-09-25, option 1. This file fixes the exact code shape for
`design.md` "Step D3b". Where they differ, this file wins. Add no type,
function, trait impl, module, feature or re-export that is not listed.

**Escalate instead of improvising.** If a shape cannot be achieved as written
(it does not compile, it loses coverage that this file does not allow you to
drop, or it needs an Arena method not listed here), stop that item. Write
`impl/conflict-d3b.md` with the item, the exact obstacle (file:line, error or
test) and the options you see, then end your turn. Do not add an Arena method
yourself.

## 1. Deletions

- `crates/arena0-sdk/src/testing.rs`, `crates/arena0-sdk/src/testing/`
  (all four files) and `crates/arena0-sdk/tests/fixtures.rs`.
- `crates/arena0-sdk-macros/src/arena0_test.rs`, its `mod` line and the
  `#[proc_macro_attribute]` that calls `arena0_test::expand` in
  `sdk-macros/src/lib.rs`.
- Every `testing` item in the SDK `lib.rs`/`prelude.rs` (`pub mod testing`,
  re-exports, and doc links to them).
- `DivergenceDiagnostic`, `DivergenceKind` and `trace/divergence.rs` in
  `arena0-protocol`, with their re-exports, if nothing outside the deleted
  harness uses them. If something does, escalate.
- Cargo dev-dependencies and features that only the deleted code needed.

## 2. Native host shims (`crates/arena0-sdk/src/effects.rs`)

The non-`wasm32` branches existed only to feed the harness. Replace every one of
them with a call to one private function in `effects.rs`:

```rust
/// Host imports exist only inside the Wasm guest. Native builds compile the
/// SDK for pure program tests, which never reach a host import.
#[cfg(not(target_arch = "wasm32"))]
#[cold]
fn native_host_unavailable(import: &'static str) -> ! {
    panic!("arena0 host import `{import}` is only available inside the Wasm guest")
}
```

- This covers `host_log`, `host_random`, `host_broadcast`, `host_set_timer_spec`,
  `host_guest_sign`, `host_end_session`, `host_abort_session`, `host_fail`,
  and the state read/write/len shims. Keep `let _ = (…);` only where it is
  needed to silence unused-argument warnings, before the call. The `import`
  argument is the `imports::*` name.
- Delete `fake_sign`.
- If a native test outside the deleted harness then panics in
  `native_host_unavailable`, it drove a handler natively. Move it to an Arena
  test (section 4), or list it as dropped with a reason (section 5). Do not
  restore a native sink.

The SDK unit tests in `context.rs` that read `crate::testing::drain_effects()`
or call `ctx.sign(...)` natively are deleted. The real guest path in
`arena0-sandbox/tests/real_guest_runtime.rs` and the program Arena tests cover
broadcast encoding and signing. List them in the coverage table as "dropped:
native effect sink only".

## 3. Arena API additions (`crates/arena0-tests/src/arena.rs`)

Exactly these, and nothing else:

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

- `callout` waits with the same deadline, drain and termination-panic
  behavior as `respond_bytes`. It reads the newest `SessionMessage::CalloutRequested`
  without removing it, and parses `context` with `serde_json::from_slice`.
- `respond_rejected` shares the wait loop with `respond_bytes`. Move the loop
  into one private `async fn submit(self, data: Vec<u8>) -> Result<(), ExecError>`
  on `Expect`, which does not remove the `CalloutRequested` event. Then
  `respond_bytes` is `submit`, panic on `Err`, remove the event; and
  `respond_rejected` is `submit`, match
  `Err(ExecError::InputRejected(reason)) => reason`, and panic on anything
  else. This is the one allowed private helper.
- `query` and `view` send the command, await the reply and panic with the
  participant index on error. `view` returns the `View` and drops the version.
- `Arena::params` already exists for custom params. Add nothing else.

## 4. Where moved tests go

- Keep native `#[test]`s in `programs/*` and `examples/minimal-program` that
  call pure program functions and types (rules, scoring, legality, allocation,
  codecs, `outcome`/`view`/`writer` on a constructed `Shared`, schema metadata).
  They must not construct a `Context`, `LocalContext` or `CalloutContext`, and
  must not call a handler.
- Every test that drove handlers (`TestHarness`, `Scenario`, `BilateralPair`,
  `#[arena0::test]`) is either:
  - rewritten into the program's existing Arena file,
    `crates/arena0-tests/tests/<program>_{bilateral,multiparty,trilateral}.rs`
    (`minimal-program` gets `crates/arena0-tests/tests/minimal_program.rs`,
    if it has an Arena-worthy scenario), or
  - dropped, with a reason from section 5.
- One Arena run per distinct setup. A run checks, in order: callout contexts
  (`callout`), rejected answers (`respond_rejected`, asserting the reason text
  the program returns), views/queries mid-game, then the outcome and receipts.
  Add a new run only for a setup that differs (params, participant count, or a
  different path through the game).
- Assert observable results only: the callout index and context fields,
  rejection reasons, `View` slots, query JSON, outcomes, receipt
  verification. Never decode shared state bytes in a test.
- Where a native pure test already proves a rule (for example the payoff
  matrix), do not re-prove it on a Host.

## 5. Allowed drop reasons (use one of these verbatim)

- "dropped: native harness only". The test asserted harness mechanics
  (replay reports, coverage, pending ledger, delivery schedules, fault status).
- "dropped: native effect sink only". The test read effects or signatures that
  only the native sink produced.
- "dropped: internal state". The test asserted shared-state fields with no
  observable projection. Only allowed if the same rule is visible through a
  callout, view, query or outcome that another kept test asserts. Name that
  test.

## 6. Docs and routing

- Rewrite the program-testing sections of `docs/programming.md` and
  `docs/development.md`: pure logic gets native unit tests, and behavior gets
  Arena tests in `crates/arena0-tests`, which need `just build-programs` first.
  Grep `docs/`, `README.md`, the SDK crate docs and the `cargo-arena0` templates
  for `testing::`, `TestHarness`, `Scenario`, `arena0::test` and `BilateralPair`,
  and remove or rewrite every hit. If `cargo arena0 new` generates a harness
  test, make it generate one native pure test instead.
- `scripts/check-affected.sh` / `justfile`: a change under `programs/<p>` must
  run that program's Arena test target(s) in `arena0-tests`. Read the routing
  contract in `docs/development.md`, apply it, and update the contract text.

## Gates

`cargo fmt --all`, `just build-programs`, `just check`, `just test`. Time
`just test` and `cargo test -p arena0-tests` (the program suite) with `time`.

Write `impl/report-d3b.md` with:

- one line per section above;
- deleted lines (production / test), from `git diff --stat`;
- the coverage table, with every deleted or moved test mapped to
  "native pure test <name>", "Arena test <file>::<name>" or one section-5 drop
  reason;
- the wall times;
- any deviation (there should be none);
- the gate tails.

Do not commit.
