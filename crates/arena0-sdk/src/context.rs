//! Dispatch context threaded through every program handler.
//!
//! [`Context`] owns shared and participant-local state, transport identity, and
//! session identity.
//! [`Effects`] provides host side-effect methods (broadcast, timer, callout, etc.).
//! Effects are emitted through an effect handle:
//!
//! ```ignore
//! ctx.effects().broadcast(&msg);
//! ```
//!
//! Every mutating handler receives the same context. It may update either state
//! value and emit effects; the runtime decides whether the complete dispatch
//! result is accepted at the dispatch boundary.

use arena0_crypto::{CryptoError, HashAlgorithm, SignScheme};
use arena0_protocol::{Committed, Ensemble, LogLevel, Participant, PeerId};
use borsh::BorshSerialize;
use std::marker::PhantomData;

use crate::effects;
use crate::timer::IntoTimerEffect;
use crate::{Arena0TypedCalloutRequest, Program, Transition};

/// Dispatch context passed to every mutating handler.
///
/// One context owns both replicated shared state and participant-local state.
/// Handlers may update either state and emit effects during the same semantic
/// dispatch. The Host treats both values as one dispatch result, restoring both
/// on rejection or fault and persisting/promoting them atomically at the
/// applicable agreement boundary.
pub struct Context<Shared, Local = ()> {
    shared: Shared,
    local: Local,
    peer_id: PeerId,
    remote_peer: Option<PeerId>,
    participant: Option<Participant>,
    /// The confirmed session ensemble, captured from `SessionStarted`. Agreed at
    /// negotiation (every participant BLS-signs the activation proposal that carries the
    /// full participant set), so it is authoritative for participant resolution without
    /// being re-hashed into program shared state.
    committed_ensemble: Option<Ensemble<Committed>>,
}

impl<Shared: std::fmt::Debug, Local: std::fmt::Debug> std::fmt::Debug for Context<Shared, Local> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Context")
            .field("shared", &self.shared)
            .field("local", &self.local)
            .field("peer_id", &self.peer_id)
            .field("participant", &self.participant)
            .finish_non_exhaustive()
    }
}

impl<Shared, Local> Context<Shared, Local> {
    #[doc(hidden)]
    pub fn __new(shared: Shared, local: Local, peer_id: PeerId) -> Self {
        Self {
            shared,
            local,
            peer_id,
            remote_peer: None,
            participant: None,
            committed_ensemble: None,
        }
    }

    #[doc(hidden)]
    pub fn __into_parts(self) -> (Shared, Local, PeerId) {
        (self.shared, self.local, self.peer_id)
    }

    /// Shared state visible to every participant.
    pub fn shared(&self) -> &Shared {
        &self.shared
    }

    /// Mutably borrow replicated shared state for this event.
    pub fn shared_mut(&mut self) -> &mut Shared {
        &mut self.shared
    }

    /// Mutably borrow both state values for a coordinated update.
    pub fn state_mut(&mut self) -> (&mut Shared, &mut Local) {
        (&mut self.shared, &mut self.local)
    }

    /// Apply a shared-state mutation through a closure.
    pub fn mutate_shared<R>(&mut self, f: impl FnOnce(&mut Shared) -> R) -> R {
        f(&mut self.shared)
    }

    #[doc(hidden)]
    pub fn __set_participant(&mut self, participant: Participant) {
        self.participant = Some(participant);
    }

    #[doc(hidden)]
    pub fn __set_remote_peer(&mut self, peer: PeerId) {
        self.remote_peer = Some(peer);
    }

    #[doc(hidden)]
    pub fn __set_committed_ensemble(&mut self, ensemble: Ensemble<Committed>) {
        self.committed_ensemble = Some(ensemble);
    }

    /// The confirmed session ensemble, or `None` before a session starts. The
    /// dispatch glue uses this to resolve a message sender to its participant.
    #[doc(hidden)]
    #[must_use]
    pub fn __committed_ensemble(&self) -> Option<&Ensemble<Committed>> {
        self.committed_ensemble.as_ref()
    }

    /// Shared reference to local, participant-private state.
    pub fn local(&self) -> &Local {
        &self.local
    }

    /// Mutable reference to local, participant-private state.
    pub fn local_mut(&mut self) -> &mut Local {
        &mut self.local
    }

    /// The identity of the local node running this program instance.
    pub fn identity(&self) -> PeerId {
        self.peer_id
    }

    /// The confirmed session ensemble: the agreed participant set in canonical
    /// (sorted `PeerId`) order, as sealed by negotiation.
    ///
    /// Use it to map between [`Participant`] and [`PeerId`], iterate co-participants
    /// ([`others`](Ensemble::others)), or size buffers ([`len`](Ensemble::len)).
    /// The [`me`](Self::me) helper resolves this node in any active session;
    /// [`peer`](Self::peer) and [`other`](Self::other) remain bilateral
    /// shorthands.
    ///
    /// # Panics
    /// Panics if called before a session is established.
    pub fn ensemble(&self) -> &Ensemble<Committed> {
        self.committed_ensemble
            .as_ref()
            .expect("no session ensemble (called before the session started)")
    }

    /// The remote transport peer in this bilateral session.
    ///
    /// # Panics
    /// Panics if called before a session is established.
    pub fn peer(&self) -> PeerId {
        self.remote_peer.expect("no bilateral session established")
    }

    /// This node's participant identity in the active session.
    ///
    /// The participant is assigned by the committed ensemble's canonical peer
    /// ordering, so this works for bilateral and N-party programs. The
    /// [`peer`](Self::peer) and [`other`](Self::other) methods remain bilateral
    /// helpers.
    ///
    /// # Panics
    /// Panics if called before a session is established.
    pub fn me(&self) -> Participant {
        self.participant
            .expect("participant not assigned (no active session)")
    }

    /// The other participant in a bilateral session.
    ///
    /// # Panics
    /// Panics if called before a session is established.
    pub fn other(&self) -> Participant {
        self.me().other()
    }

    /// The local session role for role enums derived from participant identity.
    ///
    /// Programs can implement `From<Participant>` for their domain role enum and
    /// then use `ctx.role::<Role>()` instead of repeating participant-index
    /// checks in handlers.
    pub fn role<Role>(&self) -> Role
    where
        Role: From<Participant>,
    {
        Role::from(self.me())
    }

    /// Convert a domain role back to its canonical session participant.
    pub fn participant<Role>(&self, role: Role) -> Participant
    where
        Role: Into<Participant>,
    {
        let participant = role.into();
        if let Some(ensemble) = self.committed_ensemble.as_ref() {
            assert!(
                ensemble.peer_at(participant).is_some(),
                "participant {} is not in the committed ensemble",
                participant.index()
            );
        } else {
            assert!(
                participant.index() < 2,
                "role maps to participant {} outside this bilateral session",
                participant.index()
            );
        }
        participant
    }

    /// Resolve a session participant to its transport identity.
    ///
    /// The committed ensemble is authoritative for N-party sessions. The
    /// bilateral fallback keeps this convenience usable while a session is
    /// being assembled in native tests.
    pub fn peer_for(&self, participant: Participant) -> PeerId {
        if let Some(ensemble) = self.committed_ensemble.as_ref() {
            return ensemble
                .peer_at(participant)
                .expect("participant is not in the committed ensemble");
        }
        if participant == self.me() {
            self.peer_id
        } else if participant == self.other() {
            self.peer()
        } else {
            panic!(
                "participant {} is not in this bilateral session",
                participant.index()
            );
        }
    }

    /// Resolve a transport peer in the active session to a participant.
    ///
    /// # Panics
    /// Panics if `peer` is outside the active bilateral ensemble.
    pub fn participant_for_peer(&self, peer: PeerId) -> Participant {
        if let Some(ensemble) = self.committed_ensemble.as_ref() {
            return ensemble
                .participant_of(&peer)
                .expect("peer is not in the committed ensemble");
        }
        if peer == self.peer_id {
            self.me()
        } else if Some(peer) == self.remote_peer {
            self.other()
        } else {
            panic!("peer {peer} is not in this session");
        }
    }

    /// Local participant index in the active session's canonical ensemble.
    ///
    /// # Panics
    /// Panics if called before a session is established.
    pub fn my_index(&self) -> usize {
        self.me().index()
    }

    /// Atomic local mutation without access to effects.
    pub fn mutate_local<R>(&mut self, f: impl FnOnce(&mut Local) -> R) -> R {
        f(&mut self.local)
    }

    /// Run a primitive helper with an immutable view of its shared field and
    /// mutable access to the program's local state.
    ///
    /// Primitive extensions use this when an operation needs to inspect the
    /// current shared protocol value while updating a local companion DTO.
    pub(crate) fn __with_shared_local<P, R>(
        &mut self,
        field: for<'b> fn(&'b Shared) -> &'b P,
        f: impl FnOnce(&P, &mut Local) -> R,
    ) -> R {
        let primitive = field(&self.shared);
        f(primitive, &mut self.local)
    }

    /// Emit an informational local diagnostic log.
    ///
    /// Logs are local telemetry, not protocol effects. They are useful in
    /// native tests and dev tooling, but they are not part of transition,
    /// effect, or shared-state convergence.
    pub fn log(&mut self, msg: &str) {
        self.log_level(LogLevel::Info, msg);
    }

    /// Emit a local diagnostic log at the given level.
    ///
    /// Prefer [`log`](Self::log) for ordinary authoring.
    pub fn log_level(&mut self, level: LogLevel, msg: &str) {
        effects::host_log(level, msg);
    }

    /// Fill `buf` with host-provided random bytes.
    ///
    pub fn random(&mut self, buf: &mut [u8]) {
        effects::host_random(buf);
    }

    /// Fill a fixed-size array with host-provided random bytes.
    ///
    pub fn random_bytes<const N: usize>(&mut self) -> [u8; N] {
        let mut buf = [0u8; N];
        effects::host_random(&mut buf);
        buf
    }

    /// Pure synchronous cryptographic helpers.
    pub fn crypto(&self) -> Crypto {
        Crypto
    }

    /// Convert a primitive-produced peer message into a detached broadcast output.
    pub fn primitive_output<T>(&mut self, msg: T) -> PrimitiveOutput<T> {
        PrimitiveOutput {
            message: msg,
            _marker: PhantomData,
        }
    }

    /// Convert multiple primitive peer messages into a detached broadcast batch.
    pub fn primitive_outputs<T>(
        &mut self,
        messages: impl IntoIterator<Item = T>,
    ) -> PrimitiveOutputs<T> {
        PrimitiveOutputs {
            outputs: messages
                .into_iter()
                .map(|msg| PrimitiveOutput {
                    message: msg,
                    _marker: PhantomData,
                })
                .collect(),
        }
    }

    /// Build a primitive handle that can operate on both shared and local
    /// state. Generated `#[arena0::state]` accessors use this hidden method.
    #[doc(hidden)]
    pub fn __primitive_field<P>(
        &mut self,
        field: for<'b> fn(&'b mut Shared) -> &'b mut P,
        shared_field: for<'b> fn(&'b Shared) -> &'b P,
    ) -> PrimitiveField<'_, Shared, Local, P> {
        PrimitiveField {
            ctx: self,
            field,
            shared_field,
            _marker: PhantomData,
        }
    }

    /// Build a route-aware primitive handle that can operate on both shared
    /// and local state.
    #[doc(hidden)]
    pub fn __primitive_field_routed<P, Route>(
        &mut self,
        field: for<'b> fn(&'b mut Shared) -> &'b mut P,
        shared_field: for<'b> fn(&'b Shared) -> &'b P,
    ) -> PrimitiveField<'_, Shared, Local, P, Route> {
        PrimitiveField {
            ctx: self,
            field,
            shared_field,
            _marker: PhantomData,
        }
    }

    /// Apply one lifecycle transition after the handler has mutated state.
    #[doc(hidden)]
    pub fn __apply_transition<P>(&mut self, transition: Transition<P::Phase>)
    where
        P: Program<Shared = Shared>,
    {
        match transition {
            Transition::Stay => {}
            Transition::To(phase) => P::__set_phase(&mut self.shared, phase),
            Transition::End => {
                let outcome = P::outcome(&self.shared);
                let bytes = borsh::to_vec(&outcome).expect("outcome serialization failed");
                effects::host_end_session(&bytes);
            }
            Transition::Abort(reason) => effects::host_abort_session(reason.as_str()),
        }
    }

    /// Borrow the side-effect handle without access to state.
    ///
    /// The returned [`Effects`] handle can emit dispatch effects, but cannot
    /// read or mutate program state.
    pub fn effects(&mut self) -> Effects<'_, Shared> {
        Effects {
            _marker: PhantomData,
        }
    }
}

/// Host side-effect handle passed to [`Context::effects`].
///
/// Provides methods to broadcast messages, set timers, and request callouts.
/// Lifecycle transitions belong to the enclosing [`Context`], not this effect
/// handle. The handle does not expose program state to the caller.
pub struct Effects<'a, Shared> {
    _marker: PhantomData<&'a Shared>,
}

/// Owned callout effect builder.
///
/// The builder captures request bytes without retaining a mutable borrow of
/// [`Context`]. Programs call [`dispatch`](Self::dispatch) explicitly.
#[derive(Debug, Clone)]
#[must_use = "callout builders must be dispatched"]
pub struct CalloutBuilder<T = ()> {
    callout_index: u32,
    context: Vec<u8>,
    expected_type: Option<String>,
    _output: PhantomData<fn() -> T>,
}

/// Owned external signing effect builder.
#[derive(Debug, Clone)]
#[must_use = "sign builders must be dispatched"]
pub struct SignBuilder {
    scheme: SignScheme,
    data: Vec<u8>,
    expected_type: Option<String>,
}

/// Primitive message routing metadata.
///
/// Generated primitive accessors use a zero-sized route type to wrap primitive
/// messages in the program-level `Message` envelope before emitting a send
/// effect. [`RawPrimitiveRoute`] is the escape hatch for primitives that already
/// produce the final wire message.
pub trait PrimitiveRoute<T> {
    type Message: BorshSerialize;

    fn wrap(message: T) -> Self::Message;
}

/// Identity routing for primitive outputs that already are peer messages.
#[derive(Debug, Clone, Copy, Default)]
pub struct RawPrimitiveRoute;

impl<T: BorshSerialize> PrimitiveRoute<T> for RawPrimitiveRoute {
    type Message = T;

    fn wrap(message: T) -> Self::Message {
        message
    }
}

/// Detached output produced by a protocol primitive.
///
/// A primitive output captures the primitive peer message without holding a
/// mutable context borrow, so authors can write
/// `ctx.commitments().commit(choice)?.broadcast()`. Messaging is
/// broadcast-only: every output goes to all participants and enters the
/// agreed trace at a canonical position.
#[derive(Debug, Clone)]
#[must_use = "primitive outputs must be broadcast, batched, or intentionally dropped"]
pub struct PrimitiveOutput<T, Route = RawPrimitiveRoute> {
    message: T,
    _marker: PhantomData<Route>,
}

/// Batch of detached primitive outputs that share a flush boundary.
///
/// A dispatch may emit at most one broadcast. Keep a batch only when selecting
/// one output or inspecting several; [`Self::broadcast`] and
/// [`Self::broadcast_via`] reject multi-output batches before emitting anything.
#[derive(Debug, Clone, Default)]
#[must_use = "primitive output batches must be sent or intentionally dropped"]
pub struct PrimitiveOutputs<T, Route = RawPrimitiveRoute> {
    outputs: Vec<PrimitiveOutput<T, Route>>,
}

/// Generated handle for a primitive field in any mutating handler.
///
/// A primitive field belongs to shared state, while a primitive operation may
/// also need to maintain a participant-local companion value and emit a
/// message. Keeping all three capabilities on one handle lets a primitive
/// complete its shared and local updates within one `Context` dispatch.
pub struct PrimitiveField<'a, Shared, Local, P, Route = RawPrimitiveRoute> {
    ctx: &'a mut Context<Shared, Local>,
    field: for<'b> fn(&'b mut Shared) -> &'b mut P,
    shared_field: for<'b> fn(&'b Shared) -> &'b P,
    _marker: PhantomData<Route>,
}

impl<Shared, Local, P, Route> std::fmt::Debug for PrimitiveField<'_, Shared, Local, P, Route> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PrimitiveField").finish_non_exhaustive()
    }
}

/// Pure cryptographic helpers available inside deterministic handlers.
#[derive(Debug, Clone, Copy, Default)]
pub struct Crypto;

impl Crypto {
    /// Hash `data` synchronously inside the guest.
    #[must_use]
    pub fn hash(&self, algorithm: HashAlgorithm, data: &[u8]) -> [u8; 32] {
        arena0_crypto::hash(algorithm, data)
    }

    /// Verify a signature synchronously inside the guest.
    pub fn verify(
        &self,
        scheme: SignScheme,
        key: &[u8],
        data: &[u8],
        signature: &[u8],
    ) -> Result<bool, CryptoError> {
        arena0_crypto::verify(scheme, key, data, signature)
    }
}

impl<Shared: std::fmt::Debug> std::fmt::Debug for Effects<'_, Shared> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Effects").finish_non_exhaustive()
    }
}

impl<T> CalloutBuilder<T> {
    /// Override the expected output type name recorded for diagnostics.
    pub fn expected_type(mut self, expected_type: impl Into<String>) -> Self {
        self.expected_type = Some(expected_type.into());
        self
    }

    /// Emit the callout effect.
    pub fn dispatch(self) {
        effects::host_callout_raw(
            self.callout_index,
            &self.context,
            self.expected_type.as_deref(),
        );
    }
}

impl SignBuilder {
    /// Override the expected output type name recorded for diagnostics.
    pub fn expected_type(mut self, expected_type: impl Into<String>) -> Self {
        self.expected_type = Some(expected_type.into());
        self
    }

    /// Emit the sign effect.
    pub fn dispatch(self) {
        effects::host_sign(self.scheme, &self.data, self.expected_type.as_deref());
    }
}

impl<T, Route> PrimitiveOutput<T, Route>
where
    Route: PrimitiveRoute<T>,
{
    /// Broadcast this primitive output to every participant.
    pub fn broadcast(self) {
        let msg = Route::wrap(self.message);
        let msg_bytes = borsh::to_vec(&msg).expect("primitive message serialization");
        effects::host_broadcast(&msg_bytes);
    }
}

impl<T, Route> PrimitiveOutput<T, Route> {
    /// Wrap this primitive output in a program-level peer-message envelope and
    /// broadcast it to every participant.
    pub fn broadcast_via<W: BorshSerialize>(self, wrap: impl FnOnce(T) -> W) {
        let msg_bytes =
            borsh::to_vec(&wrap(self.message)).expect("primitive envelope serialization");
        effects::host_broadcast(&msg_bytes);
    }
}

impl<T, Route> PrimitiveOutputs<T, Route> {
    /// Create a batch from already detached outputs.
    pub fn from_outputs(outputs: impl IntoIterator<Item = PrimitiveOutput<T, Route>>) -> Self {
        Self {
            outputs: outputs.into_iter().collect(),
        }
    }

    /// Number of outputs in the batch.
    #[must_use]
    pub fn len(&self) -> usize {
        self.outputs.len()
    }

    /// Whether the batch contains no outputs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty()
    }

    /// Wrap and broadcast the sole output in this batch.
    ///
    /// # Panics
    ///
    /// Panics when the batch contains more than one output. The host enforces
    /// the same cardinality at the dispatch boundary, and rejecting before the
    /// first effect prevents a partially emitted batch.
    pub fn broadcast_via<W: BorshSerialize>(self, mut wrap: impl FnMut(T) -> W) {
        assert!(
            self.outputs.len() <= 1,
            "one dispatch can emit at most one broadcast"
        );
        if let Some(output) = self.outputs.into_iter().next() {
            output.broadcast_via(&mut wrap);
        }
    }

    /// Iterate over detached outputs for custom wrapping.
    pub fn into_outputs(self) -> impl Iterator<Item = PrimitiveOutput<T, Route>> {
        self.outputs.into_iter()
    }
}

impl<T, Route> PrimitiveOutputs<T, Route>
where
    Route: PrimitiveRoute<T>,
{
    /// Broadcast the sole output in this batch to every participant.
    ///
    /// # Panics
    ///
    /// Panics when the batch contains more than one output. The host accepts
    /// at most one broadcast per dispatch.
    pub fn broadcast(self) {
        assert!(
            self.outputs.len() <= 1,
            "one dispatch can emit at most one broadcast"
        );
        if let Some(output) = self.outputs.into_iter().next() {
            output.broadcast();
        }
    }
}

impl<Shared, Local, P, Route> PrimitiveField<'_, Shared, Local, P, Route> {
    /// Return the participant identity associated with this dispatch.
    pub fn me(&self) -> Participant {
        self.ctx.me()
    }

    /// Mutate the primitive field as part of the current dispatch.
    pub fn mutate<R>(&mut self, f: impl FnOnce(&mut P) -> R) -> R {
        let field = self.field;
        self.ctx.mutate_shared(|shared| f(field(shared)))
    }

    /// Read the shared primitive while mutating participant-local state.
    pub fn with_shared_local<R>(&mut self, f: impl FnOnce(&P, &mut Local) -> R) -> R {
        let field = self.shared_field;
        self.ctx.__with_shared_local(field, f)
    }

    /// Mutate the program's participant-local state.
    pub fn mutate_local<R>(&mut self, f: impl FnOnce(&mut Local) -> R) -> R {
        self.ctx.mutate_local(f)
    }

    /// Fill a fixed-size array with host-provided random bytes.
    pub fn random_bytes<const N: usize>(&mut self) -> [u8; N] {
        self.ctx.random_bytes()
    }

    /// Convert one primitive message into a detached output.
    pub fn output<T>(&mut self, msg: T) -> PrimitiveOutput<T, Route> {
        PrimitiveOutput {
            message: msg,
            _marker: PhantomData,
        }
    }

    /// Convert multiple primitive messages into a detached output batch.
    pub fn outputs<T>(
        &mut self,
        messages: impl IntoIterator<Item = T>,
    ) -> PrimitiveOutputs<T, Route> {
        PrimitiveOutputs {
            outputs: messages
                .into_iter()
                .map(|msg| PrimitiveOutput {
                    message: msg,
                    _marker: PhantomData,
                })
                .collect(),
        }
    }
}

impl<Shared> Effects<'_, Shared> {
    /// Broadcast a borsh-serialized message to every participant.
    ///
    /// A dispatch may emit at most one broadcast. When a local event emits it,
    /// the message establishes the next proposal and its writer is authorized
    /// by the committed shared state from before that dispatch. When
    /// [`arena0_protocol::Event::SessionStarted`] or
    /// [`arena0_protocol::Event::MessageReceived`] emits it, the message is a
    /// deferred successor to that event and its writer is authorized by the
    /// post-dispatch shared state.
    ///
    /// The originating participant does not dispatch its own message again:
    /// it must apply every intended shared and local state change during the
    /// event that emits the broadcast. Other participants receive the message
    /// through their normal message dispatch after the proposal is agreed.
    pub fn broadcast<T: BorshSerialize>(&mut self, msg: &T) {
        let msg_bytes = borsh::to_vec(msg).expect("message serialization");
        effects::host_broadcast(&msg_bytes);
    }

    /// Build a callout effect.
    pub fn callout<A>(&mut self, req: A) -> CalloutBuilder<A::Output>
    where
        A: Arena0TypedCalloutRequest + serde::Serialize,
    {
        CalloutBuilder {
            callout_index: req.callout_index(),
            context: serde_json::to_vec(&req).expect("callout context serialization failed"),
            expected_type: req.expected_type_name().map(str::to_string),
            _output: PhantomData,
        }
    }

    /// Schedule a timer.
    ///
    /// Untyped timers use a delay and the unit marker:
    /// `ctx.effects().set_timer(1000, ())`.
    ///
    /// Typed timers use a program enum plus a duration:
    /// `ctx.effects().set_timer(Timer::TurnDeadline, Duration::from_secs(60))`.
    pub fn set_timer<A, B>(&mut self, timer: A, schedule: B)
    where
        (A, B): IntoTimerEffect,
    {
        let spec = (timer, schedule).into_timer_spec();
        effects::host_set_timer_spec(&spec);
    }

    /// Build an external signing effect.
    pub fn sign(&mut self, scheme: SignScheme, data: &[u8]) -> SignBuilder {
        SignBuilder {
            scheme,
            data: data.to_vec(),
            expected_type: Some("Vec<u8>".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Primitive;
    use sha2::Digest as _;
    use tiny_keccak::Hasher as _;

    #[derive(Default, Debug, Clone, borsh::BorshSerialize, borsh::BorshDeserialize)]
    struct TestState {
        shared_value: u32,
    }

    impl Primitive for TestState {}

    fn make_ctx() -> Context<TestState> {
        Context::__new(TestState::default(), (), PeerId([0; 32]))
    }

    #[derive(serde::Serialize)]
    struct TestCallout;

    impl crate::Arena0CalloutRequest for TestCallout {
        fn callout_index(&self) -> u32 {
            7
        }

        fn expected_type_name(&self) -> Option<&'static str> {
            Some("test::Output")
        }
    }

    impl crate::Arena0TypedCalloutRequest for TestCallout {
        type Output = String;
    }

    fn assert_test_callout(effect: &arena0_protocol::Effect) {
        match effect {
            arena0_protocol::Effect::Callout {
                callout_index,
                expected_type,
                ..
            } => {
                assert_eq!(*callout_index, 7);
                assert_eq!(expected_type.as_deref(), Some("test::Output"));
            }
            other => panic!("expected callout effect, got {other:?}"),
        }
    }

    #[test]
    fn callout_builder_preserves_expected_type() {
        let mut ctx = make_ctx();
        crate::testing::drain_effects();

        ctx.effects()
            .callout(TestCallout)
            .expected_type("test::Output")
            .dispatch();
        let effects = crate::testing::drain_effects();
        assert_eq!(effects.len(), 1);
        assert_test_callout(&effects[0]);
    }

    #[test]
    fn sign_builder_records_expected_type() {
        let mut ctx = make_ctx();
        crate::testing::drain_effects();

        ctx.effects()
            .sign(SignScheme::Ed25519, b"payload")
            .dispatch();

        let effects = crate::testing::drain_effects();
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            arena0_protocol::Effect::Sign {
                scheme,
                data,
                expected_type,
                ..
            } => {
                assert_eq!(*scheme, SignScheme::Ed25519);
                assert_eq!(data, b"payload");
                assert_eq!(expected_type.as_deref(), Some("Vec<u8>"));
            }
            other => panic!("expected sign effect, got {other:?}"),
        }
    }

    #[test]
    fn effects_broadcast_records_broadcast_effect() {
        let mut ctx = make_ctx();
        ctx.__set_participant(Participant::new(0));
        crate::testing::drain_effects();

        ctx.effects().broadcast(&42u32);

        let effects = crate::testing::drain_effects();
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            arena0_protocol::Effect::Broadcast { data } => {
                let value: u32 = borsh::from_slice(data).unwrap();
                assert_eq!(value, 42);
            }
            other => panic!("expected broadcast effect, got {other:?}"),
        }
    }

    #[test]
    fn primitive_output_broadcasts_after_releasing_context() {
        let mut ctx = make_ctx();
        crate::testing::drain_effects();

        let output = ctx.primitive_output(42u32);
        output.broadcast();

        let effects = crate::testing::drain_effects();
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            arena0_protocol::Effect::Broadcast { data } => {
                let value: u32 = borsh::from_slice(data).unwrap();
                assert_eq!(value, 42);
            }
            other => panic!("expected broadcast effect, got {other:?}"),
        }
    }

    #[test]
    fn primitive_output_can_broadcast_through_program_envelope() {
        #[derive(borsh::BorshSerialize, borsh::BorshDeserialize, PartialEq, Debug)]
        enum Envelope {
            Primitive(u32),
        }

        let mut ctx = make_ctx();
        crate::testing::drain_effects();

        ctx.primitive_output(42u32)
            .broadcast_via(Envelope::Primitive);

        let effects = crate::testing::drain_effects();
        assert_eq!(effects.len(), 1);
        match &effects[0] {
            arena0_protocol::Effect::Broadcast { data } => {
                let value: Envelope = borsh::from_slice(data).unwrap();
                assert_eq!(value, Envelope::Primitive(42));
            }
            other => panic!("expected broadcast effect, got {other:?}"),
        }
    }

    #[test]
    #[should_panic(expected = "one dispatch can emit at most one broadcast")]
    fn primitive_outputs_reject_multiple_broadcasts() {
        let mut ctx = make_ctx();
        crate::testing::drain_effects();

        let outputs = ctx.primitive_outputs([1u32, 2u32]);
        assert_eq!(outputs.len(), 2);
        outputs.broadcast();
    }

    #[test]
    fn crypto_hash_matches_supported_algorithms() {
        let crypto = Crypto;
        let data = b"arena0";

        assert_eq!(
            crypto.hash(HashAlgorithm::Blake3, data),
            *blake3::hash(data).as_bytes()
        );

        let sha256 = sha2::Sha256::digest(data);
        let mut expected_sha256 = [0u8; 32];
        expected_sha256.copy_from_slice(&sha256);
        assert_eq!(crypto.hash(HashAlgorithm::Sha256, data), expected_sha256);

        let mut expected_keccak = [0u8; 32];
        let mut keccak = tiny_keccak::Keccak::v256();
        keccak.update(data);
        keccak.finalize(&mut expected_keccak);
        assert_eq!(crypto.hash(HashAlgorithm::Keccak256, data), expected_keccak);
    }

    #[test]
    fn crypto_verifies_ed25519_synchronously() {
        use ed25519_dalek::Signer as _;

        let signing_key = ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]);
        let verifying_key = signing_key.verifying_key();
        let data = b"sign me";
        let signature = signing_key.sign(data);
        let crypto = Crypto;

        assert!(
            crypto
                .verify(
                    SignScheme::Ed25519,
                    verifying_key.as_bytes(),
                    data,
                    &signature.to_bytes(),
                )
                .expect("valid ed25519 verification"),
        );
        assert!(
            !crypto
                .verify(
                    SignScheme::Ed25519,
                    verifying_key.as_bytes(),
                    b"tampered",
                    &signature.to_bytes(),
                )
                .expect("invalid signatures return false"),
        );
    }

    #[test]
    fn crypto_rejects_malformed_ed25519_inputs() {
        let crypto = Crypto;

        let key_err = crypto
            .verify(SignScheme::Ed25519, &[1, 2, 3], b"data", &[0u8; 64])
            .expect_err("short key should fail before verification");
        assert!(matches!(
            key_err,
            CryptoError::InvalidKeyLength {
                expected: 32,
                actual: 3
            }
        ));

        let sig_err = crypto
            .verify(SignScheme::Ed25519, &[0u8; 32], b"data", &[0u8; 8])
            .expect_err("short signature should be rejected");
        assert!(matches!(sig_err, CryptoError::InvalidSignature));
    }
}
