//! Dispatch context threaded through every program handler.
//!
//! [`Context`] owns shared and participant-local state, transport identity, and
//! session identity.
//! [`Effects`] provides host side-effect methods (broadcast, timer, etc.).
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
use crate::{Program, Transition};

mod sealed {
    pub trait Sealed {}
}

/// Which handler a [`Ctx`] serves. Sealed: the SDK defines every mode.
pub trait Mode: sealed::Sealed {}

/// A mode whose handler may change local state and emit effects.
///
/// Emission takes the handle, so only an effect-bearing context of the same mode can emit.
/// A callout has no effect handle:
///
/// ```compile_fail
/// fn emit(mut ctx: arena0::CalloutContext<(), ()>) {
///     let _ = ctx.effects();
/// }
/// ```
///
/// A local handler cannot select agreed broadcast semantics:
///
/// ```compile_fail
/// use arena0::{AgreedMode, EffectMode, LocalContext};
/// fn emit(ctx: &mut LocalContext<(), ()>) {
///     <AgreedMode as EffectMode>::__broadcast(&mut ctx.effects(), Vec::new());
/// }
/// ```
///
/// Nor can safe code build an agreed context to borrow its handle:
///
/// ```compile_fail
/// fn emit(ctx: &arena0::CalloutContext<(), ()>) {
///     let mut forged = arena0::Context::<(), ()>::__new((), (), ctx.identity());
///     forged.effects().broadcast(&0u8);
/// }
/// ```
pub trait EffectMode: Mode + Sized {
    /// The result of one broadcast: `()` for agreed handlers,
    /// `Result<(), BroadcastError>` for local handlers.
    type Broadcast;

    #[doc(hidden)]
    fn __broadcast<Shared>(
        effects: &mut Effects<'_, Shared, Self>,
        bytes: Vec<u8>,
    ) -> Self::Broadcast;

    /// Emit every message in order.
    #[doc(hidden)]
    fn __broadcast_all<Shared>(
        effects: &mut Effects<'_, Shared, Self>,
        bytes: impl IntoIterator<Item = Vec<u8>>,
    ) -> Self::Broadcast;
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

impl sealed::Sealed for AgreedMode {}
impl sealed::Sealed for LocalMode {}
impl sealed::Sealed for ReadMode {}
impl Mode for AgreedMode {}
impl Mode for LocalMode {}
impl Mode for ReadMode {}

impl EffectMode for AgreedMode {
    type Broadcast = ();

    fn __broadcast<Shared>(_: &mut Effects<'_, Shared, Self>, bytes: Vec<u8>) -> Self::Broadcast {
        let queued = effects::host_broadcast(&bytes);
        debug_assert!(queued.is_ok(), "an agreed broadcast always queues");
    }

    fn __broadcast_all<Shared>(
        effects: &mut Effects<'_, Shared, Self>,
        bytes: impl IntoIterator<Item = Vec<u8>>,
    ) -> Self::Broadcast {
        for b in bytes {
            Self::__broadcast(effects, b);
        }
    }
}

impl EffectMode for LocalMode {
    type Broadcast = Result<(), BroadcastError>;

    fn __broadcast<Shared>(_: &mut Effects<'_, Shared, Self>, bytes: Vec<u8>) -> Self::Broadcast {
        effects::host_broadcast(&bytes)
    }

    fn __broadcast_all<Shared>(
        _: &mut Effects<'_, Shared, Self>,
        bytes: impl IntoIterator<Item = Vec<u8>>,
    ) -> Self::Broadcast {
        let mut result = Ok(());
        for b in bytes {
            result = result.and(effects::host_broadcast(&b));
        }
        result
    }
}

/// Dispatch context for one program handler. `M` selects what the handler may
/// do: see [`AgreedMode`], [`LocalMode`] and [`ReadMode`].
pub struct Ctx<Shared, Local = (), M = AgreedMode> {
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
    _mode: PhantomData<M>,
}

/// Dispatch context passed to every mutating handler.
///
/// One context owns both replicated shared state and participant-local state.
/// Handlers may update either state and emit effects during the same semantic
/// dispatch. The Host treats both values as one dispatch result, restoring both
/// on rejection or fault and persisting/promoting them atomically at the
/// applicable agreement boundary.
pub type Context<Shared, Local = ()> = Ctx<Shared, Local, AgreedMode>;

/// Dispatch context for local handlers.
///
/// A local event may not change agreed shared state, so this context owns the
/// shared value privately and exposes it read-only. It provides the participant,
/// local-state, effect, random, log, and primitive author helpers, plus the
/// synchronous [`sign`](Self::sign) host call. It has no way to extract or
/// reconstruct a mutable shared image.
pub type LocalContext<Shared, Local = ()> = Ctx<Shared, Local, LocalMode>;

/// Read-only context for `callout`.
pub type CalloutContext<Shared, Local = ()> = Ctx<Shared, Local, ReadMode>;

impl<Shared: std::fmt::Debug, Local: std::fmt::Debug, M> std::fmt::Debug for Ctx<Shared, Local, M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ctx")
            .field("shared", &self.shared)
            .field("local", &self.local)
            .field("peer_id", &self.peer_id)
            .field("participant", &self.participant)
            .finish_non_exhaustive()
    }
}

impl<Shared, Local, M: Mode> Ctx<Shared, Local, M> {
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

    #[doc(hidden)]
    pub fn __into_parts(self) -> (Shared, Local) {
        (self.shared, self.local)
    }

    /// Convert into the read-only view `callout` receives. Moves the fields; nothing is copied.
    #[doc(hidden)]
    pub fn __read(self) -> CalloutContext<Shared, Local> {
        Ctx {
            shared: self.shared,
            local: self.local,
            peer_id: self.peer_id,
            remote_peer: self.remote_peer,
            participant: self.participant,
            committed_ensemble: self.committed_ensemble,
            _mode: PhantomData,
        }
    }

    /// Shared state visible to every participant.
    pub fn shared(&self) -> &Shared {
        &self.shared
    }

    /// Shared reference to local, participant-private state.
    pub fn local(&self) -> &Local {
        &self.local
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
    /// bilateral fallback applies only before a committed ensemble is
    /// installed.
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
}

impl<Shared, Local, M: EffectMode> Ctx<Shared, Local, M> {
    /// Mutable reference to local, participant-private state.
    pub fn local_mut(&mut self) -> &mut Local {
        &mut self.local
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
    /// Logs are local telemetry, not protocol effects. They are useful in dev
    /// tooling, but they are not part of transition,
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
    ) -> PrimitiveField<'_, Shared, Local, P, RawPrimitiveRoute, M> {
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
    ) -> PrimitiveField<'_, Shared, Local, P, Route, M> {
        PrimitiveField {
            ctx: self,
            field,
            shared_field,
            _marker: PhantomData,
        }
    }

    /// Borrow the side-effect handle without access to state.
    ///
    /// The returned [`Effects`] handle can emit dispatch effects, but cannot
    /// read or mutate program state.
    pub fn effects(&mut self) -> Effects<'_, Shared, M> {
        Effects {
            _marker: PhantomData,
        }
    }
}

impl<Shared, Local> Ctx<Shared, Local, AgreedMode> {
    /// Build the context for one agreed dispatch.
    ///
    /// # Safety
    ///
    /// Only the dispatch of an agreed event may build this context. Its
    /// effect handle emits with agreed semantics, so a context built inside a
    /// callout or local handler would let that handler broadcast outside its
    /// mode. Generated dispatch glue meets this precondition.
    #[doc(hidden)]
    pub unsafe fn __new(shared: Shared, local: Local, peer_id: PeerId) -> Self {
        Self {
            shared,
            local,
            peer_id,
            remote_peer: None,
            participant: None,
            committed_ensemble: None,
            _mode: PhantomData,
        }
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
}

impl<Shared, Local> Ctx<Shared, Local, LocalMode> {
    /// Build the context for one local dispatch.
    ///
    /// # Safety
    ///
    /// The caller must pass this dispatch's committed shared image and its
    /// durable local image, and the shared value must be exactly the one the
    /// Host will compare against. `callout` derives from the shared image, so
    /// passing any other value would let a local handler expose a replacement
    /// view to `callout`, influencing a callout from state the Host never
    /// commits. Generated dispatch glue meets this precondition; a test may deliberately violate it only to exercise
    /// the byte guard that rejects the resulting dispatch.
    #[doc(hidden)]
    pub unsafe fn __new(shared: Shared, local: Local, peer_id: PeerId) -> Self {
        Self {
            shared,
            local,
            peer_id,
            remote_peer: None,
            participant: None,
            committed_ensemble: None,
            _mode: PhantomData,
        }
    }

    /// Sign `payload` synchronously with the participant's host key.
    ///
    /// The host builds a versioned, execution-bound preimage from the dispatch
    /// coordinates and returns both that exact preimage and its signature; both
    /// schemes are deterministic, so re-running the handler after a crash
    /// produces the same signature. Available only in local handlers
    /// (`InputReceived`, `TimerFired`) whose program declared a `Sign`
    /// capability for the requested scheme.
    pub fn sign(&mut self, scheme: SignScheme, payload: &[u8]) -> Signed {
        let (signed_bytes, signature) = effects::host_guest_sign(scheme, payload);
        Signed {
            signed_bytes,
            signature,
        }
    }
}

/// Failure to emit a broadcast.
///
/// A broadcast that returns an error queued nothing; the program may retry or
/// carry on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcastError {
    /// The durable outgoing queue has reached its bound.
    QueueFull,
}

impl std::fmt::Display for BroadcastError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::QueueFull => write!(f, "the outgoing message queue is full"),
        }
    }
}

impl std::error::Error for BroadcastError {}

/// Host side-effect handle passed to [`Context::effects`].
///
/// Provides methods to broadcast messages and set timers. Lifecycle
/// transitions belong to the enclosing [`Context`], not this effect handle.
/// The handle does not expose program state to the caller.
pub struct Effects<'a, Shared, M = AgreedMode> {
    _marker: PhantomData<(&'a Shared, M)>,
}

/// Exact bytes and signature returned by one synchronous guest signing call.
///
/// The guest receives both so it can persist or forward the exact preimage the
/// host signed; the host never lets the guest guess what was signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    /// The exact versioned, execution-bound preimage the host signed.
    pub signed_bytes: Vec<u8>,
    /// The signature over `signed_bytes`.
    pub signature: Vec<u8>,
}

/// Primitive message routing metadata.
///
/// Generated primitive accessors use a zero-sized route type to wrap primitive
/// messages in the program-level `Message` envelope before broadcasting them. [`RawPrimitiveRoute`] is the escape hatch for primitives that already
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
/// [`Self::broadcast`] and [`Self::broadcast_via`] broadcast every output in
/// the batch, in order, through the enclosing context's effect handle.
#[derive(Debug, Clone, Default)]
#[must_use = "primitive output batches must be sent or intentionally dropped"]
pub struct PrimitiveOutputs<T, Route = RawPrimitiveRoute> {
    outputs: Vec<PrimitiveOutput<T, Route>>,
}

/// Generated handle for a primitive field.
///
/// An agreed handler's field is mutable; a local handler's field is
/// read-only and has no [`PrimitiveField::mutate`] method.
pub struct PrimitiveField<'a, Shared, Local, P, Route = RawPrimitiveRoute, M = AgreedMode> {
    ctx: &'a mut Ctx<Shared, Local, M>,
    field: for<'b> fn(&'b mut Shared) -> &'b mut P,
    shared_field: for<'b> fn(&'b Shared) -> &'b P,
    _marker: PhantomData<Route>,
}

impl<Shared, Local, P, Route, Mode> std::fmt::Debug
    for PrimitiveField<'_, Shared, Local, P, Route, Mode>
{
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

impl<Shared, M> std::fmt::Debug for Effects<'_, Shared, M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Effects").finish_non_exhaustive()
    }
}

impl<Shared, M: EffectMode> Effects<'_, Shared, M> {
    /// Broadcast a borsh-serialized message to every participant.
    ///
    /// The message is appended to this participant's durable outgoing queue. A
    /// later dispatch authors the head when the `writer` projection selects
    /// this participant; the author applies its own message through the same
    /// `on_message` dispatch every receiver runs.
    ///
    /// In an agreed handler this cannot fail: an agreed handler never observes
    /// the local queue, and if the agreed step would overflow the queue the
    /// Host fails the session instead of signing it. In a local handler it
    /// returns [`BroadcastError::QueueFull`] when the queue is full and queues
    /// nothing.
    pub fn broadcast<T: BorshSerialize>(&mut self, msg: &T) -> M::Broadcast {
        M::__broadcast(self, borsh::to_vec(msg).expect("message serialization"))
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
}

impl<T, Route: PrimitiveRoute<T>> PrimitiveOutput<T, Route> {
    /// Broadcast this primitive output to every participant through the
    /// enclosing context's effect handle.
    pub fn broadcast<Shared, M: EffectMode>(
        self,
        effects: &mut Effects<'_, Shared, M>,
    ) -> M::Broadcast {
        let msg = Route::wrap(self.message);
        M::__broadcast(
            effects,
            borsh::to_vec(&msg).expect("primitive message serialization"),
        )
    }
}

impl<T, Route> PrimitiveOutput<T, Route> {
    /// Wrap this primitive output in a program-level peer-message envelope and
    /// broadcast it to every participant through the enclosing context's
    /// effect handle.
    pub fn broadcast_via<W: BorshSerialize, Shared, M: EffectMode>(
        self,
        effects: &mut Effects<'_, Shared, M>,
        wrap: impl FnOnce(T) -> W,
    ) -> M::Broadcast {
        M::__broadcast(
            effects,
            borsh::to_vec(&wrap(self.message)).expect("primitive envelope serialization"),
        )
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

    /// Wrap and broadcast every output in this batch through the enclosing
    /// context's effect handle.
    pub fn broadcast_via<W: BorshSerialize, Shared, M: EffectMode>(
        self,
        effects: &mut Effects<'_, Shared, M>,
        mut wrap: impl FnMut(T) -> W,
    ) -> M::Broadcast {
        M::__broadcast_all(
            effects,
            self.outputs
                .into_iter()
                .map(|output| {
                    borsh::to_vec(&wrap(output.message)).expect("primitive envelope serialization")
                })
                .collect::<Vec<_>>(),
        )
    }

    /// Iterate over detached outputs for custom wrapping.
    pub fn into_outputs(self) -> impl Iterator<Item = PrimitiveOutput<T, Route>> {
        self.outputs.into_iter()
    }
}

impl<T, Route: PrimitiveRoute<T>> PrimitiveOutputs<T, Route> {
    /// Broadcast every output in this batch to every participant through the
    /// enclosing context's effect handle.
    pub fn broadcast<Shared, M: EffectMode>(
        self,
        effects: &mut Effects<'_, Shared, M>,
    ) -> M::Broadcast {
        M::__broadcast_all(
            effects,
            self.outputs
                .into_iter()
                .map(|output| {
                    borsh::to_vec(&Route::wrap(output.message))
                        .expect("primitive message serialization")
                })
                .collect::<Vec<_>>(),
        )
    }
}

impl<Shared, Local, P, Route, M: EffectMode> PrimitiveField<'_, Shared, Local, P, Route, M> {
    /// Return the participant identity associated with this dispatch.
    pub fn me(&self) -> Participant {
        self.ctx.me()
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

impl<Shared, Local, P, Route> PrimitiveField<'_, Shared, Local, P, Route, AgreedMode> {
    /// Mutate the primitive field as part of the current dispatch.
    ///
    /// Available only on an agreed handler's field; a local handler's field is
    /// read-only.
    pub fn mutate<R>(&mut self, f: impl FnOnce(&mut P) -> R) -> R {
        let field = self.field;
        self.ctx.mutate_shared(|shared| f(field(shared)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::Digest as _;
    use tiny_keccak::Hasher as _;

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
