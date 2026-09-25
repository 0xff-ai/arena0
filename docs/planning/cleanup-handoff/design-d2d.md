# D2d: sandbox projection and initialize calls take plain arguments (F6)

Context: `impl/audit-shape.md` F6. Each of `InitializeCall`, `QueryCall`,
`ViewCall`, `OutcomeCall` and `WriterCall` in `crates/arena0-sandbox/src/call.rs`
exists only to be built by the caller and turned into the ABI input by
`into_input`. The shapes below are decided. Implement them exactly, and add no
type, function, trait impl or re-export that is not listed.

**Escalate instead of improvising.** If a shape cannot be achieved as written
(it does not compile, it changes observable behavior, or it needs something not
listed here), stop that item. Write `impl/conflict-d2d.md` with the item, the
exact obstacle (file:line and the error) and the options you see, then end your
turn. Do not pick another shape.

## Target API: `impl LoadedProgram` in `crates/arena0-sandbox/src/engine/runtime.rs`

```rust
/// Execute initialization in a fresh guest instance.
pub fn initialize(&self, params: JsonBytes) -> Result<InitializedState, SandboxError>;

/// Execute the pure next-writer projection in a fresh guest instance.
pub fn writer(
    &self,
    shared: &SharedStateBytes,
    session: &Ensemble<Committed>,
) -> Result<GuestWriterResult, SandboxError>;

/// Execute one read-only query in a fresh guest instance. `query_index`
/// selects the advertised query schema.
pub fn query(
    &self,
    shared: &SharedStateBytes,
    session: &Ensemble<Committed>,
    query_index: u32,
    query: JsonBytes,
) -> Result<GuestProjectionResult, SandboxError>;

/// Execute one read-only viewport projection in a fresh guest instance.
pub fn view(
    &self,
    shared: &SharedStateBytes,
    session: &Ensemble<Committed>,
    viewport: JsonBytes,
) -> Result<GuestProjectionResult, SandboxError>;

/// Execute the pure terminal-outcome projection in a fresh guest instance.
pub fn outcome(
    &self,
    shared: &SharedStateBytes,
    session: &Ensemble<Committed>,
) -> Result<GuestOutcomeResult, SandboxError>;
```

`resident` and `ProgramInstance::dispatch(DispatchCall)` do not change.

## Bodies

- Each of `writer`, `query`, `view` and `outcome` first calls
  `self.validate_shared_state(shared)?`. It then builds its ABI input inline
  from the arguments:
  - `WriterInput { shared: shared.clone() }`;
  - `QueryInput::try_new(shared.clone(), serialize(session)?, query_index, query.into_bytes())`;
  - `ViewInput::try_new(shared.clone(), serialize(session)?, viewport.into_bytes())`;
  - `OutcomeInput::try_new(shared.clone(), serialize(session)?)`.

  Map each `try_new` error with
  `.map_err(|error| SandboxError::input_limit(error.to_string()))`, exactly as
  `into_input` does today. `serialize` is `crate::call::serialize`, which stays.
- `initialize` builds
  `InitInput::try_new(params.into_bytes()).map_err(|error| SandboxError::input_limit(error.to_string()))?`
  and is otherwise unchanged.
- `query` looks up the schema with `query_index` before anything else, as it
  does today. The explicit `self.validate_shared_state(&input.shared)?` in
  `query` goes away, because the shared bound is now checked once at the top.
  The output index check compares against the `query_index` argument.
- `writer` uses `session.len()` for its ensemble-bound check.
- `fn project`: remove its `shared: impl FnOnce(&I) -> &SharedStateBytes`
  parameter and the `validate_shared_state` call inside it. The callers already
  validated `shared`. The new signature is
  `fn project<I, O>(&self, kind: CallKind, export: &str, operation: &str, input: I) -> Result<(O, u64), SandboxError>`.
  Update its doc comment: "Run one read-only projection in a fresh instance
  and reject any guest effect. Callers validate the shared-state bound first."

## Deletions and module contents

- `call.rs`: delete `InitializeCall`, `QueryCall`, `ViewCall`, `OutcomeCall`,
  `WriterCall` and their impls. Keep `DispatchKind`, `DispatchParts`,
  `DispatchCall` (unchanged), `serialize`, and the existing test. Update the
  module doc to: "Typed input for resident dispatch and the shared input
  encoder."
- `lib.rs`: the re-export becomes `pub use call::DispatchCall;`.
- `runtime.rs`: fix the `use` lists. `JsonBytes`, `Ensemble`, `Committed` and
  `serialize` come in, and the call structs go out.

## Callers to migrate (mechanical; no new helpers)

Use the new signatures directly. Where a caller cloned the state or ensemble
only to build a call struct, pass a borrow instead.

- `crates/arena0-node/src/execution/guest.rs` (`query` ~l.164, `view` ~l.178,
  `writer` inside `writer_for_shared`, `outcome` ~l.696)
- `crates/arena0-node/src/execution/actor.rs` (`initialize` ~l.361)
- `crates/arena0-node/src/execution/tests.rs`
- `crates/arena0-daemon/src/server.rs` (`initialize` ~l.1022, `view` ~l.3468)
- `crates/arena0-sandbox/tests/real_guest_runtime.rs`
- `crates/arena0-tests/src/arena.rs`, `crates/arena0-tests/src/fixtures.rs`,
  `crates/arena0-tests/tests/receipt_recovery.rs`
- Any other caller the compiler finds. List them in the report.

Docs: if `docs/api/` or `docs/technical-overview.md` names any of the deleted
call structs, update the text to the new method signatures.

## Gates

`cargo fmt --all`, `just check`,
`cargo test -p arena0-sandbox -p arena0-node -p arena0-daemon`, `just test`.
Paste the tails into `impl/report-d2d.md`, with one line per section above
and any deviation (there should be none). Do not commit.
