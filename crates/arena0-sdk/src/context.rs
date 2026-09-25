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
            ctx: PrimitiveCtx::Mutable(self),
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
            ctx: PrimitiveCtx::Mutable(self),
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
    pub fn effects(&mut self) -> Effects<'_, Shared, AgreedEffects> {
        Effects {
            _marker: PhantomData,
        }
    }

    /// The read-only view a program's `callout` function receives.
    #[doc(hidden)]
    #[must_use]
    pub fn __callout_context(&self) -> CalloutContext<'_, Shared, Local> {
        CalloutContext {
            shared: &self.shared,
            local: &self.local,
            peer_id: self.peer_id,
            remote_peer: self.remote_peer,
            participant: self.participant,
            committed_ensemble: self.committed_ensemble.as_ref(),
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

/// Dispatch context for local handlers.
///
/// A local event may not change agreed shared state, so this context owns the
/// shared value privately and exposes it read-only. It provides the participant,
/// local-state, effect, random, log, and primitive author helpers, plus the
/// synchronous [`sign`](Self::sign) host call. It has no way to extract or
/// reconstruct a mutable shared image.
pub struct LocalContext<Shared, Local = ()> {
    shared: Shared,
    local: Local,
    peer_id: PeerId,
    remote_peer: Option<PeerId>,
    participant: Option<Participant>,
    committed_ensemble: Option<Ensemble<Committed>>,
}

impl<Shared: std::fmt::Debug, Local: std::fmt::Debug> std::fmt::Debug
    for LocalContext<Shared, Local>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalContext")
            .field("shared", &self.shared)
            .field("local", &self.local)
            .field("peer_id", &self.peer_id)
            .field("participant", &self.participant)
            .finish_non_exhaustive()
    }
}

impl<Shared, Local> LocalContext<Shared, Local> {
    /// Build the context for one local dispatch.
    ///
    /// # Safety
    ///
    /// The caller must pass this dispatch's committed shared image and its
    /// durable local image, and the shared value must be exactly the one the
    /// Host will compare against. `callout` derives from the shared image, so
    /// passing any other value would let a local handler expose a replacement
    /// view to `callout`, influencing a callout from state the Host never
    /// commits. Generated dispatch glue and the native dispatch harness meet
    /// this precondition; a test may deliberately violate it only to exercise
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
        }
    }

    /// Consume the context and return the local state.
    ///
    /// Generated dispatch glue uses this to store the accepted local image; the
    /// shared image is written back from the original Host bytes, never from
    /// this context.
    #[doc(hidden)]
    pub fn __into_local(self) -> Local {
        self.local
    }

    /// Shared state visible to every participant, read-only.
    pub fn shared(&self) -> &Shared {
        &self.shared
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

    /// The confirmed session ensemble, or `None` before a session starts.
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

    /// The confirmed session ensemble in canonical order.
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
    pub fn my_index(&self) -> usize {
        self.me().index()
    }

    /// Atomic local mutation without access to effects.
    pub fn mutate_local<R>(&mut self, f: impl FnOnce(&mut Local) -> R) -> R {
        f(&mut self.local)
    }

    /// Run a primitive helper with an immutable view of its shared field and
    /// mutable access to the program's local state.
    pub(crate) fn __with_shared_local<P, R>(
        &mut self,
        field: for<'b> fn(&'b Shared) -> &'b P,
        f: impl FnOnce(&P, &mut Local) -> R,
    ) -> R {
        let primitive = field(&self.shared);
        f(primitive, &mut self.local)
    }

    /// Emit an informational local diagnostic log.
    pub fn log(&mut self, msg: &str) {
        self.log_level(LogLevel::Info, msg);
    }

    /// Emit a local diagnostic log at the given level.
    pub fn log_level(&mut self, level: LogLevel, msg: &str) {
        effects::host_log(level, msg);
    }

    /// Fill `buf` with host-provided random bytes.
    pub fn random(&mut self, buf: &mut [u8]) {
        effects::host_random(buf);
    }

    /// Fill a fixed-size array with host-provided random bytes.
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

    /// Build a read-only primitive handle for a generated `#[arena0::state]`
    /// field.
    ///
    /// The returned handle has no shared-mutation method, so a local handler
    /// cannot change agreed shared state through a primitive.
    #[doc(hidden)]
    pub fn __primitive_field<P>(
        &mut self,
        field: for<'b> fn(&'b mut Shared) -> &'b mut P,
        shared_field: for<'b> fn(&'b Shared) -> &'b P,
    ) -> PrimitiveField<'_, Shared, Local, P, RawPrimitiveRoute, LocalPrimitive> {
        PrimitiveField {
            ctx: PrimitiveCtx::Local(self),
            field,
            shared_field,
            _marker: PhantomData,
        }
    }

    /// Build a route-aware read-only primitive handle for a generated field.
    #[doc(hidden)]
    pub fn __primitive_field_routed<P, Route>(
        &mut self,
        field: for<'b> fn(&'b mut Shared) -> &'b mut P,
        shared_field: for<'b> fn(&'b Shared) -> &'b P,
    ) -> PrimitiveField<'_, Shared, Local, P, Route, LocalPrimitive> {
        PrimitiveField {
            ctx: PrimitiveCtx::Local(self),
            field,
            shared_field,
            _marker: PhantomData,
        }
    }

    /// Borrow the side-effect handle without access to state.
    pub fn effects(&mut self) -> Effects<'_, Shared, LocalEffects> {
        Effects {
            _marker: PhantomData,
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

    /// The read-only view a program's `callout` function receives.
    #[doc(hidden)]
    #[must_use]
    pub fn __callout_context(&self) -> CalloutContext<'_, Shared, Local> {
        CalloutContext {
            shared: &self.shared,
            local: &self.local,
            peer_id: self.peer_id,
            remote_peer: self.remote_peer,
            participant: self.participant,
            committed_ensemble: self.committed_ensemble.as_ref(),
        }
    }
}

/// The read-only context a program's `callout` function receives.
///
/// It borrows the shared and local state, so a callout can inspect state but
/// never change it. Both [`Context`] and [`LocalContext`] produce one.
pub struct CalloutContext<'a, Shared, Local = ()> {
    shared: &'a Shared,
    local: &'a Local,
    peer_id: PeerId,
    remote_peer: Option<PeerId>,
    participant: Option<Participant>,
    committed_ensemble: Option<&'a Ensemble<Committed>>,
}

impl<Shared: std::fmt::Debug, Local: std::fmt::Debug> std::fmt::Debug
    for CalloutContext<'_, Shared, Local>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CalloutContext")
            .field("shared", &self.shared)
            .field("local", &self.local)
            .field("participant", &self.participant)
            .finish_non_exhaustive()
    }
}

impl<Shared, Local> CalloutContext<'_, Shared, Local> {
    /// Shared state visible to every participant.
    pub fn shared(&self) -> &Shared {
        self.shared
    }

    /// Local, participant-private state.
    pub fn local(&self) -> &Local {
        self.local
    }

    /// The identity of the local node running this program instance.
    pub fn identity(&self) -> PeerId {
        self.peer_id
    }

    /// The confirmed session ensemble in canonical order.
    ///
    /// # Panics
    /// Panics if called before a session is established.
    pub fn ensemble(&self) -> &Ensemble<Committed> {
        self.committed_ensemble
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
        if let Some(ensemble) = self.committed_ensemble {
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
    pub fn peer_for(&self, participant: Participant) -> PeerId {
        if let Some(ensemble) = self.committed_ensemble {
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
        if let Some(ensemble) = self.committed_ensemble {
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
    pub fn my_index(&self) -> usize {
        self.me().index()
    }
}

/// Host side-effect handle passed to [`Context::effects`].
///
/// Provides methods to broadcast messages and set timers. Lifecycle
/// transitions belong to the enclosing [`Context`], not this effect handle.
/// The handle does not expose program state to the caller.
pub struct Effects<'a, Shared, Mode = AgreedEffects> {
    _marker: PhantomData<(&'a Shared, Mode)>,
}

/// Marker for an agreed handler's effect handle.
///
/// An agreed handler never observes the local outgoing queue, so its
/// [`Effects::broadcast`] is infallible. See [`LocalEffects`].
#[derive(Debug, Clone, Copy)]
pub struct AgreedEffects;

/// Marker for a local handler's effect handle.
///
/// A local handler observes the bounded outgoing queue, so its
/// [`Effects::broadcast`] returns [`BroadcastError`].
#[derive(Debug, Clone, Copy)]
pub struct LocalEffects;

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

/// Marker for a primitive field obtained from an agreed handler's [`Context`].
#[derive(Debug, Clone, Copy)]
pub struct MutablePrimitive;

/// Marker for a primitive field obtained from a local handler's
/// [`LocalContext`].
#[derive(Debug, Clone, Copy)]
pub struct LocalPrimitive;

/// The context reference behind a primitive field.
enum PrimitiveCtx<'a, Shared, Local> {
    Mutable(&'a mut Context<Shared, Local>),
    Local(&'a mut LocalContext<Shared, Local>),
}

impl<Shared, Local> PrimitiveCtx<'_, Shared, Local> {
    fn me(&self) -> Participant {
        match self {
            Self::Mutable(ctx) => ctx.me(),
            Self::Local(ctx) => ctx.me(),
        }
    }

    fn __with_shared_local<P, R>(
        &mut self,
        field: for<'b> fn(&'b Shared) -> &'b P,
        f: impl FnOnce(&P, &mut Local) -> R,
    ) -> R {
        match self {
            Self::Mutable(ctx) => ctx.__with_shared_local(field, f),
            Self::Local(ctx) => ctx.__with_shared_local(field, f),
        }
    }

    fn mutate_local<R>(&mut self, f: impl FnOnce(&mut Local) -> R) -> R {
        match self {
            Self::Mutable(ctx) => ctx.mutate_local(f),
            Self::Local(ctx) => ctx.mutate_local(f),
        }
    }

    fn random_bytes<const N: usize>(&mut self) -> [u8; N] {
        match self {
            Self::Mutable(ctx) => ctx.random_bytes(),
            Self::Local(ctx) => ctx.random_bytes(),
        }
    }
}

/// Generated handle for a primitive field.
///
/// An agreed handler's field is mutable ([`MutablePrimitive`]); a local
/// handler's field is read-only ([`LocalPrimitive`]) and has no
/// [`PrimitiveField::mutate`] method.
pub struct PrimitiveField<'a, Shared, Local, P, Route = RawPrimitiveRoute, Mode = MutablePrimitive>
{
    ctx: PrimitiveCtx<'a, Shared, Local>,
    field: for<'b> fn(&'b mut Shared) -> &'b mut P,
    shared_field: for<'b> fn(&'b Shared) -> &'b P,
    _marker: PhantomData<(Route, Mode)>,
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

impl<Shared: std::fmt::Debug, Mode> std::fmt::Debug for Effects<'_, Shared, Mode> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Effects").finish_non_exhaustive()
    }
}

/// A mode-specific broadcast sink used by detached primitive outputs.
///
/// An agreed handler's sink is infallible; a local handler's sink returns
/// [`BroadcastError`]. Passing the enclosing context's `effects()` handle makes
/// misuse a type error.
#[doc(hidden)]
pub trait BroadcastSink {
    /// The mode-specific result of emitting one or more broadcasts.
    type Output;

    /// Emit one already-encoded message.
    fn emit_broadcast(&mut self, bytes: Vec<u8>) -> Self::Output;

    /// An empty batch result.
    fn empty_output() -> Self::Output;

    /// Combine two per-output results in order.
    fn combine(first: Self::Output, second: Self::Output) -> Self::Output;
}

impl<Shared> BroadcastSink for Effects<'_, Shared, AgreedEffects> {
    type Output = ();

    fn emit_broadcast(&mut self, bytes: Vec<u8>) -> Self::Output {
        let queued = effects::host_broadcast(&bytes);
        debug_assert!(queued.is_ok(), "an agreed broadcast always queues");
    }

    fn empty_output() -> Self::Output {}

    fn combine(_first: Self::Output, _second: Self::Output) -> Self::Output {}
}

impl<Shared> BroadcastSink for Effects<'_, Shared, LocalEffects> {
    type Output = Result<(), BroadcastError>;

    fn emit_broadcast(&mut self, bytes: Vec<u8>) -> Self::Output {
        effects::host_broadcast(&bytes)
    }

    fn empty_output() -> Self::Output {
        Ok(())
    }

    fn combine(first: Self::Output, second: Self::Output) -> Self::Output {
        first.and(second)
    }
}

impl<T, Route> PrimitiveOutput<T, Route>
where
    Route: PrimitiveRoute<T>,
{
    /// Broadcast this primitive output to every participant through the
    /// enclosing context's effect handle.
    pub fn broadcast<S: BroadcastSink>(self, sink: &mut S) -> S::Output {
        let msg = Route::wrap(self.message);
        let msg_bytes = borsh::to_vec(&msg).expect("primitive message serialization");
        sink.emit_broadcast(msg_bytes)
    }
}

impl<T, Route> PrimitiveOutput<T, Route> {
    /// Wrap this primitive output in a program-level peer-message envelope and
    /// broadcast it to every participant through the enclosing context's
    /// effect handle.
    pub fn broadcast_via<W: BorshSerialize, S: BroadcastSink>(
        self,
        sink: &mut S,
        wrap: impl FnOnce(T) -> W,
    ) -> S::Output {
        let msg_bytes =
            borsh::to_vec(&wrap(self.message)).expect("primitive envelope serialization");
        sink.emit_broadcast(msg_bytes)
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
    pub fn broadcast_via<W: BorshSerialize, S: BroadcastSink>(
        self,
        sink: &mut S,
        mut wrap: impl FnMut(T) -> W,
    ) -> S::Output {
        let mut result = S::empty_output();
        for output in self.outputs {
            let next = output.broadcast_via(&mut *sink, &mut wrap);
            result = S::combine(result, next);
        }
        result
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
    /// Broadcast every output in this batch to every participant through the
    /// enclosing context's effect handle.
    pub fn broadcast<S: BroadcastSink>(self, sink: &mut S) -> S::Output {
        let mut result = S::empty_output();
        for output in self.outputs {
            let next = output.broadcast(&mut *sink);
            result = S::combine(result, next);
        }
        result
    }
}

impl<Shared, Local, P, Route, Mode> PrimitiveField<'_, Shared, Local, P, Route, Mode> {
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

impl<Shared, Local, P, Route> PrimitiveField<'_, Shared, Local, P, Route, MutablePrimitive> {
    /// Mutate the primitive field as part of the current dispatch.
    ///
    /// Available only on an agreed handler's field; a local handler's field is
    /// read-only.
    pub fn mutate<R>(&mut self, f: impl FnOnce(&mut P) -> R) -> R {
        let field = self.field;
        match &mut self.ctx {
            PrimitiveCtx::Mutable(ctx) => ctx.mutate_shared(|shared| f(field(shared))),
            PrimitiveCtx::Local(_) => unreachable!("a local primitive field is read-only"),
        }
    }
}

impl<Shared> Effects<'_, Shared, AgreedEffects> {
    /// Broadcast a borsh-serialized message to every participant.
    ///
    /// The message is appended to this participant's durable outgoing queue. A
    /// later dispatch authors the head when the `writer` projection selects
    /// this participant; the author applies its own message through the same
    /// `on_message` dispatch every receiver runs.
    ///
    /// An agreed handler never observes the local queue, so this cannot fail;
    /// if the agreed step would overflow the queue the Host fails the session
    /// instead of signing it.
    pub fn broadcast<T: BorshSerialize>(&mut self, msg: &T) {
        let msg_bytes = borsh::to_vec(msg).expect("message serialization");
        let queued = effects::host_broadcast(&msg_bytes);
        debug_assert!(queued.is_ok(), "an agreed broadcast always queues");
    }
}

impl<Shared> Effects<'_, Shared, LocalEffects> {
    /// Broadcast a borsh-serialized message to every participant.
    ///
    /// Returns [`BroadcastError::QueueFull`] when the queue is full; nothing is
    /// queued in that case.
    pub fn broadcast<T: BorshSerialize>(&mut self, msg: &T) -> Result<(), BroadcastError> {
        let msg_bytes = borsh::to_vec(msg).expect("message serialization");
        effects::host_broadcast(&msg_bytes)
    }
}

impl<Shared, Mode> Effects<'_, Shared, Mode> {
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

    fn make_local_ctx() -> LocalContext<TestState> {
        // SAFETY: a unit test that constructs the dispatch context directly.
        unsafe { LocalContext::__new(TestState::default(), (), PeerId([0; 32])) }
    }

    #[test]
    fn synchronous_sign_returns_the_payload_and_a_deterministic_signature() {
        let mut ctx = make_local_ctx();

        let signed = ctx.sign(SignScheme::Ed25519, b"payload");
        assert_eq!(signed.signed_bytes, b"payload");
        assert_eq!(signed.signature.len(), 64);
        assert_eq!(signed, ctx.sign(SignScheme::Ed25519, b"payload"));
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
        output.broadcast(&mut ctx.effects());

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
            .broadcast_via(&mut ctx.effects(), Envelope::Primitive);

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
