# D2e: one SDK context type, parameterized by handler mode (F1)

Context: `impl/audit-shape.md` F1. `crates/arena0-sdk/src/context.rs` has three
context structs with the same six fields (`Context`, `LocalContext`,
`CalloutContext`). The first two carry ~25 identical methods each. The mode is
also encoded again by `AgreedEffects`/`LocalEffects`,
`MutablePrimitive`/`LocalPrimitive`, `PrimitiveCtx` and `BroadcastSink`.

The shapes below are decided. Implement them exactly. Do not add any type,
trait, function, method, impl or re-export that is not listed, and keep every
doc comment's meaning (move doc comments with their methods).

**Escalate instead of improvising.** If a shape cannot be achieved as written
(the compiler rejects it, a program or test changes observable behavior, or it
needs something not listed), stop that item. Write `impl/conflict-d2e.md` with
the item, the exact obstacle (file:line, the full compiler error) and the
options you see, then end your turn. Do not choose another shape.

## 1. Modes (`context.rs`)

```rust
mod sealed {
    pub trait Sealed {}
}

/// Which handler a [`Ctx`] serves. Sealed: the SDK defines every mode.
pub trait Mode: sealed::Sealed {}

/// A mode whose handler may change local state and emit effects.
pub trait EffectMode: Mode {
    /// The result of one broadcast: `()` for agreed handlers,
    /// `Result<(), BroadcastError>` for local handlers.
    type Broadcast;

    #[doc(hidden)]
    fn __broadcast(bytes: Vec<u8>) -> Self::Broadcast;

    /// Emit every message in order.
    #[doc(hidden)]
    fn __broadcast_all(bytes: impl IntoIterator<Item = Vec<u8>>) -> Self::Broadcast;
}

/// An agreed handler (`on_session_started`, `on_message`): it may change
/// shared state, and its broadcast cannot fail.
#[derive(Debug, Clone, Copy)]
pub struct AgreedMode;

/// A local handler (`on_input`, `on_timer`): shared state is read-only, a
/// broadcast can fail with [`BroadcastError::QueueFull`], and it may `sign`.
#[derive(Debug, Clone, Copy)]
pub struct LocalMode;

/// The read-only view `callout` receives.
#[derive(Debug, Clone, Copy)]
pub struct ReadMode;
```

- `impl sealed::Sealed` and `impl Mode` exist for all three markers.
  `impl EffectMode` exists for `AgreedMode` and `LocalMode` only.
- `AgreedMode`: `type Broadcast = ();`.
  - `__broadcast` calls `effects::host_broadcast(&bytes)` with
    `debug_assert!(queued.is_ok(), "an agreed broadcast always queues")`.
  - `__broadcast_all` does the same for each message.
- `LocalMode`: `type Broadcast = Result<(), BroadcastError>;`.
  - `__broadcast` is `effects::host_broadcast(&bytes)`.
  - `__broadcast_all` keeps today's batch semantics exactly: it emits every
    message and returns the first error:
    `let mut result = Ok(()); for b in bytes { result = result.and(effects::host_broadcast(&b)); } result`.
- The markers are named `*Mode` so they do not collide with the `Local`
  generic parameter and the programs' `type Local`.

## 2. `Ctx` (`context.rs`)

```rust
/// Dispatch context for one program handler. `M` selects what the handler may
/// do: see [`AgreedMode`], [`LocalMode`] and [`ReadMode`].
pub struct Ctx<Shared, Local = (), M = AgreedMode> {
    shared: Shared,
    local: Local,
    peer_id: PeerId,
    remote_peer: Option<PeerId>,
    participant: Option<Participant>,
    committed_ensemble: Option<Ensemble<Committed>>,
    _mode: PhantomData<M>,
}

/// Context for agreed handlers.
pub type Context<Shared, Local = ()> = Ctx<Shared, Local, AgreedMode>;
/// Context for local handlers.
pub type LocalContext<Shared, Local = ()> = Ctx<Shared, Local, LocalMode>;
/// Read-only context for `callout`.
pub type CalloutContext<Shared, Local = ()> = Ctx<Shared, Local, ReadMode>;
```

Keep the doc comments from today's `Context` and `LocalContext` on the aliases.
Keep the `committed_ensemble` field comment on the field. There is one `Debug`
impl, for `impl<Shared: Debug, Local: Debug, M> Debug for Ctx<Shared, Local, M>`,
with `debug_struct("Ctx")` and the same fields as today.

Impl blocks. Every method keeps today's body and doc comment. Only the block
changes:

1. `impl<Shared, Local, M: Mode> Ctx<Shared, Local, M>`:
   - reads: `shared`, `local`, `identity`, `ensemble`, `peer`, `me`, `other`,
     `role`, `participant`, `peer_for`, `participant_for_peer`, `my_index`;
   - glue: `__set_participant`, `__set_remote_peer`,
     `__set_committed_ensemble`, `__committed_ensemble`;
   - `#[doc(hidden)] pub fn __into_parts(self) -> (Shared, Local)` (it no
     longer returns the `PeerId`; no caller used it);
   - a new method:

     ```rust
     /// Convert into the read-only view `callout` receives. Moves the fields; nothing is copied.
     #[doc(hidden)]
     pub fn __read(self) -> CalloutContext<Shared, Local>
     ```
2. `impl<Shared, Local, M: EffectMode> Ctx<Shared, Local, M>`:
   - `local_mut`, `mutate_local`, `pub(crate) __with_shared_local`, `log`,
     `log_level`, `random`, `random_bytes`, `crypto`, `primitive_output`,
     `primitive_outputs`;
   - `#[doc(hidden)] pub fn __primitive_field<P>(&mut self, field, shared_field) -> PrimitiveField<'_, Shared, Local, P, RawPrimitiveRoute, M>`;
   - `#[doc(hidden)] pub fn __primitive_field_routed<P, Route>(&mut self, field, shared_field) -> PrimitiveField<'_, Shared, Local, P, Route, M>`;
   - `pub fn effects(&mut self) -> Effects<'_, Shared, M>`.
3. `impl<Shared, Local> Ctx<Shared, Local, AgreedMode>`:
   - `#[doc(hidden)] pub fn __new(shared, local, peer_id) -> Self` (safe, as
     today);
   - `shared_mut`, `state_mut`, `mutate_shared`, `__apply_transition`.
4. `impl<Shared, Local> Ctx<Shared, Local, LocalMode>`:
   - `#[doc(hidden)] pub unsafe fn __new(shared, local, peer_id) -> Self`,
     with today's `# Safety` doc unchanged;
   - `sign`.

Delete `struct LocalContext`, `struct CalloutContext<'a, …>`, both
`__callout_context` methods, `__into_local`, and the duplicated method bodies.

## 3. `Effects` (`context.rs`)

```rust
pub struct Effects<'a, Shared, M = AgreedMode> {
    _marker: PhantomData<(&'a Shared, M)>,
}

impl<Shared, M: EffectMode> Effects<'_, Shared, M> {
    /// Broadcast a borsh-serialized message to every participant.
    /// <merge today's two doc comments: the queue/writer paragraph, then
    /// "In an agreed handler this cannot fail …; in a local handler it returns
    /// [`BroadcastError::QueueFull`] when the queue is full and queues nothing.">
    pub fn broadcast<T: BorshSerialize>(&mut self, msg: &T) -> M::Broadcast {
        M::__broadcast(borsh::to_vec(msg).expect("message serialization"))
    }

    pub fn set_timer<A, B>(&mut self, timer: A, schedule: B) where (A, B): IntoTimerEffect; // body unchanged
}
```

There is one `Debug` impl, `impl<Shared, M> Debug for Effects<'_, Shared, M>`.
Delete `AgreedEffects`, `LocalEffects`, the `BroadcastSink` trait and its two
impls, and the two mode-specific `broadcast` impls.

## 4. Primitive outputs (`context.rs`)

The sink parameter becomes the effect handle itself:

```rust
impl<T, Route: PrimitiveRoute<T>> PrimitiveOutput<T, Route> {
    pub fn broadcast<Shared, M: EffectMode>(self, effects: &mut Effects<'_, Shared, M>) -> M::Broadcast;
}
impl<T, Route> PrimitiveOutput<T, Route> {
    pub fn broadcast_via<W: BorshSerialize, Shared, M: EffectMode>(
        self, effects: &mut Effects<'_, Shared, M>, wrap: impl FnOnce(T) -> W) -> M::Broadcast;
}
impl<T, Route> PrimitiveOutputs<T, Route> {
    // from_outputs, len, is_empty, into_outputs: unchanged
    pub fn broadcast_via<W: BorshSerialize, Shared, M: EffectMode>(
        self, effects: &mut Effects<'_, Shared, M>, wrap: impl FnMut(T) -> W) -> M::Broadcast;
}
impl<T, Route: PrimitiveRoute<T>> PrimitiveOutputs<T, Route> {
    pub fn broadcast<Shared, M: EffectMode>(self, effects: &mut Effects<'_, Shared, M>) -> M::Broadcast;
}
```

- The single-output methods call `M::__broadcast(bytes)`.
- The batch methods serialize every output in order (`Route::wrap`, or
  `wrap`, then `borsh::to_vec(..).expect(..)` with today's messages). They
  pass the byte vectors to `M::__broadcast_all`.

## 5. `PrimitiveField` (`context.rs`)

```rust
pub struct PrimitiveField<'a, Shared, Local, P, Route = RawPrimitiveRoute, M = AgreedMode> {
    ctx: &'a mut Ctx<Shared, Local, M>,
    field: for<'b> fn(&'b mut Shared) -> &'b mut P,
    shared_field: for<'b> fn(&'b Shared) -> &'b P,
    _marker: PhantomData<Route>,
}
```

- `impl<Shared, Local, P, Route, M: EffectMode> PrimitiveField<…, M>` holds
  `me`, `with_shared_local`, `mutate_local`, `random_bytes`, `output` and
  `outputs`. Each calls straight through `self.ctx`.
- `impl<Shared, Local, P, Route> PrimitiveField<…, AgreedMode>` holds
  `mutate`, which calls `self.ctx.mutate_shared(|shared| f(field(shared)))`.
  There is no `unreachable!`.
- The `Debug` impl stays generic over all parameters.
- Delete `PrimitiveCtx`, `MutablePrimitive` and `LocalPrimitive`.

## 6. Re-exports (`crates/arena0-sdk/src/lib.rs`, `prelude.rs`)

`pub use context::{AgreedMode, BroadcastError, CalloutContext, Context, Crypto, Ctx, EffectMode, Effects, LocalContext, LocalMode, Mode, PrimitiveField, PrimitiveOutput, PrimitiveOutputs, PrimitiveRoute, RawPrimitiveRoute, ReadMode, Signed};`

In `prelude.rs`, replace only the removed names and keep the rest as it is.
Do not add new prelude items.

## 7. `Program::callout` signature (`program.rs`) and callers

`fn callout(_ctx: &CalloutContext<Self::Shared, Self::Local>) -> Option<Self::Callout>`.
Drop the `'_` from every implementation:

- the programs `rock-paper-scissors`, `contract-net`, `chess`,
  `local-context-forge`, `prisoner-dilemma` and `vickrey-auction`;
- `crates/arena0-sdk/tests/fixtures.rs`;
- the macro `module_shell.rs:322`.

## 8. Macros (`crates/arena0-sdk-macros`)

`attr_state.rs`. There is one accessor trait and one impl set, generic over
the mode:

- `trait #accessor_trait<__Arena0Local, __Arena0Mode>`. Its methods return
  `::arena0::PrimitiveField<'_, #ident, __Arena0Local, #ty, <route>, __Arena0Mode>`.
- `impl<__Arena0Local, __Arena0Mode: ::arena0::EffectMode> #accessor_trait<__Arena0Local, __Arena0Mode> for ::arena0::Ctx<#ident, __Arena0Local, __Arena0Mode>`.
- `#generic_has_trait<__Arena0Local, __Arena0Primitive>` keeps its
  parameters. Each per-type impl becomes
  `impl<__Arena0Local, __Arena0Mode: ::arena0::EffectMode> #generic_has_trait<__Arena0Local, #ty> for ::arena0::Ctx<#ident, __Arena0Local, __Arena0Mode>`
  with `type Access<'a> = ::arena0::PrimitiveField<'a, #ident, __Arena0Local, #ty, #route_ty, __Arena0Mode>`.
- `#generic_lookup_trait<__Arena0Local>` gets one impl, for
  `impl<__Arena0Local, __Arena0Mode: ::arena0::EffectMode> … for ::arena0::Ctx<#ident, __Arena0Local, __Arena0Mode>`.
- Delete `#local_accessor_trait`, the `mutable_mode`/`local_mode` paths, and
  the doubled `accessor_methods_*`/`accessor_impls_*`/`*_generic_primitive_impls`.
  `accessor_methods_for`/`accessor_impls_for`/`generic_primitive_impls_for`
  lose their `mode`/`context` parameters and emit `__Arena0Mode` and
  `::arena0::Ctx` directly.
- Keep the "conflicts with an arena0 Context method" check. It must now cover
  every method of `Ctx` across all four impl blocks. If it lists method names,
  update the list to the union.

`program/guest_abi.rs`, the dispatch glue:

- `__arena0_make_ctx` and `__arena0_make_local_ctx` are unchanged, apart from
  the types now being aliases.
- Delete `__arena0_store_state`. Replace the callout and store section with:

  ```rust
  let (ctx, local_shared) = match ctx {
      __Arena0Dispatch::Agreed(ctx) => (ctx.__read(), None),
      __Arena0Dispatch::Local(ctx, shared_bytes) => (ctx.__read(), Some(shared_bytes)),
  };
  let callout = if status == ::arena0::CallStatus::Accepted {
      <#program_ty as ::arena0::Program>::callout(&ctx).map(|callout| { /* unchanged */ })
  } else {
      None
  };
  if status == ::arena0::CallStatus::Accepted {
      let (shared, local) = ctx.__into_parts();
      match local_shared {
          None => __arena0_store_parts(&shared, &local),
          Some(shared_bytes) => __arena0_store_local(&shared_bytes, &local),
      }
  }
  ```

  A local dispatch still writes back the original shared bytes, never the
  context's shared value. Keep that comment.
- `__arena0_local_shared_changed` and `__arena0_local_outcome` keep their
  `&::arena0::LocalContext<…>` parameter.

`program/capabilities.rs` and `program/mod.rs` detect the `Context` receiver by
its last path segment. `LocalContext` and `Context` are still the names programs
write, so nothing changes there. If a macro test breaks on the new names,
escalate.

## 9. Other callers

- `crates/arena0-primitives/src/commit_reveal.rs`:
  - the `CommitRevealAuthorExt` impl gets `Mode: arena0::EffectMode`;
  - the `CommitRevealFieldExt` impl uses `arena0::AgreedMode` in place of
    `arena0::MutablePrimitive`.
- `crates/arena0-sdk/src/testing/fixtures.rs` (the native harness, which D3b
  will delete):
  - replace `P::callout(&ctx.__callout_context())` with `let ctx = ctx.__read();`
    then `P::callout(&ctx)`;
  - replace `__into_local()` with `__into_parts().1`;
  - replace the 3-tuple `__into_parts` with the 2-tuple.
- `context.rs` unit tests: update `make_ctx`/`make_local_ctx` and the broadcast
  tests to the new types. The assertions stay.
- Programs and tests that call `.broadcast(&mut ctx.effects())` compile
  unchanged. If one doesn't, show the error in the conflict file.
- Docs: `docs/api/` and `crates/arena0-sdk/README*` if present. Replace mentions
  of `AgreedEffects`, `LocalEffects`, `BroadcastSink`, `MutablePrimitive`,
  `LocalPrimitive` and `CalloutContext<'_, …>` with the new names.

## Gates

After the SDK/macro change, rebuild the guests first (`just build-programs`).
Then run `cargo fmt --all`, `just check`, `just test`, and the macro crate's
tests (including trybuild/UI tests, if any). Report to `impl/report-d2e.md`
with one line per section, the list of files touched, any deviation (there
should be none) and the gate tails. Do not commit.
