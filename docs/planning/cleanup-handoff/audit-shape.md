# Code-shape audit: raulk/fix-dual-arch vs main

Author: integration owner, 2026-09-25. Base: merge-base `5caeea9` (= main).
Compared against: HEAD `5833c8e` plus the paused, uncommitted D2a work,
including its fix round.

Method: `ast-grep outline <crate>/src --items exports --view signatures` on both
trees. The per-item diff (489 changed exported items) is in `/tmp/shape/diff.txt`.
I then used `--view digest/expanded` on the files that changed most (SDK
`context.rs`, protocol `execution/state.rs` and `validation.rs`, node
`execution/guest.rs` and `mod.rs`, sandbox `call.rs` and `engine/runtime.rs`,
store `lib.rs`, primitives `commit_reveal.rs`).

Overall: the branch deleted far more surface than it added. Store −136/+61
exported items, macros −31/+16, protocol −104/+85. The removals follow the
decided model: no inbox, outbox, leases, terminal signatures, React, async
lowering, trait form, pending labels or multi-key keystore. The problems are in
shape that workers chose where my designs left options open. Each finding
below gives the target shape I decided.

## F1 (High) — SDK: three context types duplicate one implementation

`crates/arena0-sdk/src/context.rs` (1,531 lines; 1,301 on main):

- `Context` (l.32), `LocalContext` (l.443) and `CalloutContext` (l.793) each
  hold the same six fields.
- `Context` and `LocalContext` each implement the same ~25 methods: identity,
  ensemble, peer, me, other, role, participant, peer_for,
  participant_for_peer, my_index, local, local_mut, mutate_local, log,
  log_level, random, random_bytes, crypto, primitive_output(s),
  `__primitive_field(_routed)`, `__with_shared_local`, `__set_*`, effects,
  `__callout_context`. That is ~330 lines per type. `CalloutContext` repeats 12
  of them.
- The mode is also encoded three more times: `Effects<'a, S, Mode>` with
  `AgreedEffects`/`LocalEffects`; `PrimitiveField<…, Mode>` with
  `MutablePrimitive`/`LocalPrimitive` plus a private `PrimitiveCtx` enum; and a
  `BroadcastSink` trait (`emit_broadcast`, `empty_output`, `combine`) that
  exists only to make primitive broadcasts generic over the two return types.

Target shape (one type, one mode set):

```rust
// crates/arena0-sdk/src/context.rs
mod sealed { pub trait Sealed {} }
pub trait Mode: sealed::Sealed {}
/// Modes whose handlers may emit effects.
pub trait EffectMode: Mode {
    /// `()` for agreed handlers (queue overflow is a Host-signed Fail),
    /// `Result<(), BroadcastError>` for local handlers.
    type Broadcast;
    #[doc(hidden)] fn __broadcast(bytes: &[u8]) -> Self::Broadcast;
    #[doc(hidden)] fn __broadcast_all(messages: impl IntoIterator<Item = Vec<u8>>) -> Self::Broadcast;
}
pub struct Agreed;   // on_session_started, on_message
pub struct Local;    // on_input, on_timer
pub struct Read;     // callout derivation
impl Mode for Agreed {} impl Mode for Local {} impl Mode for Read {}
impl EffectMode for Agreed { type Broadcast = (); … }
impl EffectMode for Local { type Broadcast = Result<(), BroadcastError>; … }

pub struct Ctx<Shared, Local = (), M = Agreed> {
    shared: Shared, local: Local, peer_id: PeerId, remote_peer: Option<PeerId>,
    participant: Option<Participant>, committed_ensemble: Option<Ensemble<Committed>>,
    _mode: PhantomData<M>,
}
pub type Context<Shared, Local = ()> = Ctx<Shared, Local, Agreed>;
pub type LocalContext<Shared, Local = ()> = Ctx<Shared, Local, self::Local>;
pub type CalloutContext<Shared, Local = ()> = Ctx<Shared, Local, Read>;

impl<S, L, M: Mode> Ctx<S, L, M> {
    // doc(hidden) constructor/destructor
    pub unsafe fn __new(shared: S, local: L, peer_id: PeerId) -> Self;
    pub fn __into_parts(self) -> (S, L);
    pub fn __set_participant / __set_remote_peer / __set_committed_ensemble / __committed_ensemble;
    // reads
    pub fn shared(&self) -> &S; pub fn local(&self) -> &L;
    pub fn identity / ensemble / peer / me / other / role / participant /
           peer_for / participant_for_peer / my_index;
}
impl<S, L, M: EffectMode> Ctx<S, L, M> {
    pub fn local_mut; pub fn mutate_local; pub fn log; pub fn log_level;
    pub fn random; pub fn random_bytes; pub fn crypto;
    pub fn effects(&mut self) -> Effects<'_, S, M>;
    pub fn primitive_output / primitive_outputs;
    pub fn __primitive_field / __primitive_field_routed -> PrimitiveField<'_, S, L, P, Route, M>;
    pub fn __with_shared_local;
}
impl<S, L> Ctx<S, L, Agreed> { shared_mut, state_mut, mutate_shared, __apply_transition }
impl<S, L> Ctx<S, L, self::Local> { sign }
```

- `Effects<'a, S, M: EffectMode>` has one `broadcast(&mut self, msg: &T) -> M::Broadcast`
  and one `set_timer`.
- `PrimitiveOutput::broadcast(self, effects: &mut Effects<'_, S, M>) -> M::Broadcast`,
  `broadcast_via` likewise. `PrimitiveOutputs::broadcast`/`broadcast_via` use
  `M::__broadcast_all`.
- `PrimitiveField<'a, S, L, P, Route, M: EffectMode>`; `mutate` exists only
  for `M = Agreed`.
- Delete `LocalContext` and `CalloutContext` as separate structs,
  `AgreedEffects`, `LocalEffects`, `MutablePrimitive`, `LocalPrimitive`,
  `PrimitiveCtx`, `BroadcastSink`, `__into_local` (replaced by
  `__into_parts`) and `__callout_context`.
- The callout hook becomes `fn callout(ctx: &CalloutContext<Shared, Local>)`
  (no lifetime). The glue moves the post-dispatch images into a
  `Ctx<_, _, Read>` and takes them back with `__into_parts`.
- Primitives: `CommitRevealAuthorExt` and `CommitRevealFieldExt` are
  implemented for `PrimitiveField<…, M: EffectMode>` and
  `PrimitiveField<…, Agreed>` respectively; their method sets do not change.

## F2 (Medium) — protocol: `ExecutionState` fields mirrored three times

`execution/state.rs:246-356`: `ExecutionState` (17 persisted fields plus the
derived `receipt_overhead`), `ExecutionStateBody` (the same 17, for decode) and
`ExecutionStateBodyRef<'a>` (the same 17 by reference, added by D2a for
encode). Plus hand-written `BorshSerialize`, `BorshDeserialize` and serde
`Deserialize` impls, and `from_body`.

Target shape: persist `receipt_overhead` as an ordinary field (8 bytes), and
derive `BorshSerialize, BorshDeserialize` on `ExecutionState` itself.
`ExecutionState::decode(bytes)` is the only validated entry:
`borsh::from_slice` plus `validate_recovered`, which also checks
`receipt_overhead == ReceiptArtifact::reserved_overhead(&binding)?`. Delete
`ExecutionStateBody`, `ExecutionStateBodyRef`, `from_body`, and the
hand-written Borsh and serde `Deserialize` impls. No caller deserializes an
`ExecutionState` from JSON. Keep `Serialize` (derive) if JSON projection uses
it. Every production decode site must call `ExecutionState::decode`: check the
store, and replace any `borsh::from_slice::<ExecutionState>`. Bump the store
schema version.

## F3 (Medium) — protocol/store: three effect-budget checkers

- `validation.rs:515 check_effect_budget` (count + per-effect payload +
  aggregate bytes; used by the sandbox at emission).
- `validation.rs:542 check_aggregate_effect_bytes` (aggregate only; added by
  the paused D2a fix round).
- `store/database/execution.rs:977 validate_effect_payloads` (store-local).

Target: `check_effect_budget` is the one owner. `apply_dispatch` calls
`check_effect_budget(effects)` (this replaces the fix round's
`check_aggregate_effect_bytes`, which is deleted). The store calls
`check_effect_budget` and deletes `validate_effect_payloads`.
`validate_effects` keeps only the ordinal checks and then calls
`check_effect_budget`. `validate_effect_payload` becomes private
(`fn`, not exported) unless a caller outside the protocol remains.

## F4 (Medium) — protocol: two ways to get the proposal commitment

`SharedProposal::commitment(&self, session_id, link)` (l.159) and
`ExecutionState::proposal_commitment(&self)` (l.615), both from D2a. Its inputs
are always the state's session and agreed link. Target: keep only
`ExecutionState::proposal_commitment(&self) -> Option<StepCommitment>` and
delete `SharedProposal::commitment`. Protocol-internal callers use a private
`fn commitment_for(&self, proposal: &SharedProposal) -> StepCommitment` on
`ExecutionState`.

## F5 (Medium) — node: resident validity has two representations

`execution/mod.rs:35`: `instance: Option<ProgramInstance>` (None = rebuild) and
`resident_in_sync: bool` (D2a, 13 references), plus `reconcile_resident`.
Target: one invariant. `Some(instance)` means the instance holds exactly
`self.state`'s committed images. Every path that changes the state's images
without committing the resident calls `self.restore_resident()` immediately
(the certification path in `delivery.rs`, and any other). A failure sets
`instance = None`. Delete `resident_in_sync` and `reconcile_resident`. Dispatch
calls `self.resident_mut()?`, which rebuilds when it is `None`.

## F6 (Medium) — sandbox: every call described twice

`call.rs`: `InitializeCall`, `QueryCall`, `ViewCall`, `OutcomeCall` and
`WriterCall` each wrap the matching `arena0-program` ABI input and exist only
to convert into it (`into_input`). Target: methods on `LoadedProgram` take their
parameters directly, and the ABI input structs are built inside the sandbox:

```rust
impl LoadedProgram {
    pub fn initialize(&self, params: JsonBytes) -> Result<InitializedState, SandboxError>;
    pub fn writer(&self, shared: &SharedStateBytes, session: &Ensemble<Committed>) -> Result<GuestWriterResult, SandboxError>;
    pub fn query(&self, shared: &SharedStateBytes, session: &Ensemble<Committed>, query_index: u32, query: JsonBytes) -> Result<GuestProjectionResult, SandboxError>;
    pub fn view(&self, shared: &SharedStateBytes, session: &Ensemble<Committed>, viewport: JsonBytes) -> Result<GuestProjectionResult, SandboxError>;
    pub fn outcome(&self, shared: &SharedStateBytes, session: &Ensemble<Committed>) -> Result<GuestOutcomeResult, SandboxError>;
}
```

(`ViewCall.viewport` is `JsonBytes` today, so the signature above is exact.)
`DispatchCall` stays:
it has optional parts (`with_outgoing_len`, `with_signer`). Delete the five
structs, their `into_input`s and `call.rs::serialize` if it becomes unused.

## F7 (Low) — smaller leftovers

- `arena0-tests/src/synthetic.rs::message_event(&self, _step: u64, …)`: delete
  the unused `_step` parameter and migrate callers.
- `node/execution/guest.rs`: `writer_is(source, state)`,
  `writer_for_shared(shared, ensemble)` and `may_author()`. Keep
  `writer_for_shared` as the one computation and `may_author` as the policy;
  `writer_is` becomes `self.writer_for_shared(..)? == Some(source)` inline at
  its one or two call sites.
- `state.rs::commit_shared_inner`: rename to `install_certified` (it installs
  a certified proposal).
- D2a helper names I accept as the fixed shape: `dispatch_inner` (node),
  `project` (sandbox projections), `encode_envelope`/`decode_output` (sandbox
  envelope bounds), `prepare_instance` (sandbox bootstrap).
- `arena0-test-engine` plus the ~10-line resolver copy in the sandbox's
  `#[cfg(test)] test_support`: accepted (it avoids a dependency cycle).
- `StoreHandle` and `ExecutionStore` both expose `load_execution_request`,
  `load_activation`, `load_execution`, `load_receipt_by_id` and
  `bind_join_target`. This comes from main. The execution-scoped one is the
  writer capability, so a read duplicate is acceptable. Not changed now.

## Plan

- **D2a (resume, same worker):** F2, F3, F4, F5, and F7's `commit_shared_inner`
  rename and writer-helper change, on top of the paused tree. The fix round's
  `apply_dispatch` hash computation stays (the protocol owns the hash). Its
  `check_aggregate_effect_bytes` is replaced per F3.
- **D2d (new step, after D2a):** F6, the sandbox call API.
- **D2e (new step, after D2d, before D3b):** F1, the SDK context. It changes
  every program's handler signatures, and D3b moves those programs' tests, so
  F1 must land first.
- D2c, D2b and D3b follow as designed. Before sending each, I re-check its
  design for open choices and replace any with exact shape.
