//! The [`Program`] trait that every arena0 program implements.

use arena0_protocol::TimerPayload;
use arena0_protocol::{Ensemble, Participant, View, Viewport};
use borsh::BorshDeserialize;

use crate::{
    Arena0Callout, Arena0Phase, Arena0Query, CalloutContext, Context, LocalContext, LocalState,
    PhaseDecl, PrimitiveRouteSchema, ProgramFault, ProtocolFault, SharedState, Transition,
};

pub type ProgramTransition<P> = Transition<<P as Program>::Phase>;

/// The outcome of a shared message apply: accept (with a transition) or reject.
///
/// `Reject` is a deterministic non-application: the host rolls the dispatch
/// layer back, records no trace entry, and quarantines the message. It is not
/// a fault — faults remain fatal abort edges.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyDecision<P> {
    /// Accept the message and apply the transition.
    Accept(Transition<P>),
    /// Reject the message deterministically; the apply leaves no trace.
    Reject,
}

impl<P> From<Transition<P>> for ApplyDecision<P> {
    fn from(value: Transition<P>) -> Self {
        Self::Accept(value)
    }
}

/// Result of a shared message apply.
pub type MessageApply<P> = Result<ApplyDecision<<P as Program>::Phase>, ProtocolFault>;

/// The entry point for every arena0 program.
///
/// Programs are pure state machines: events come in, effects go out. The
/// runtime handles transport, callout/input collection, and state verification.
/// Programs never touch the network directly.
///
/// Every mutating event receives `Context<Self::Shared, Self::Local>`. A
/// callback may update both state values, emit effects, and return a lifecycle
/// transition. The Host treats the complete result atomically at the applicable
/// agreement boundary; a fault or deterministic rejection restores both state
/// values and discards provisional effects.
///
/// Module-shell programs use synchronous handlers. The open callout is derived
/// from state by [`Program::callout`]; answers are delivered through the
/// `on_input` dispatch path.
///
/// # Associated types
///
/// - **Shared**: replicated program state, annotated with `#[arena0::state]`.
///   Fields are shared-visible and `#[phase]` marks the lifecycle phase field.
/// - **Local**: participant-private session state.
/// - **Message**: the wire type exchanged between peers.
/// - **Callout**: the generated callout/callout request surface.
/// - **Input**: the typed response delivered by `on_input`.
/// - **Params**: initialization parameters provided at session creation.
///   The participant set is not a parameter; the host supplies the committed
///   [`Ensemble`] in [`Self::on_session_started`].
/// - **Outcome**: the typed terminal receipt, derived by [`outcome`](Self::outcome)
///   as a pure projection over final shared state.
pub trait Program: Sized {
    type Shared: SharedState;
    type Local: LocalState;
    type Phase: Arena0Phase;
    type Message: serde::de::DeserializeOwned
        + borsh::BorshSerialize
        + BorshDeserialize
        + borsh::BorshSchema
        + crate::ProgramValue;
    type Callout: Arena0Callout<Response = Self::Input> + crate::Arena0CalloutRequest;
    type Input: borsh::BorshSerialize + BorshDeserialize + crate::ProgramValue + 'static;
    type Params: serde::de::DeserializeOwned + serde::Serialize + crate::ProgramValue;
    type Outcome: crate::ProgramValue
        + serde::Serialize
        + borsh::BorshSerialize
        + borsh::BorshDeserialize
        + 'static;

    fn initialize(_shared: &mut Self::Shared, _params: Self::Params) -> Result<(), ProgramFault> {
        Ok(())
    }

    /// Derive the typed terminal outcome from final shared state.
    ///
    /// This is a pure projection: no [`Context`], no effects, no randomness, and
    /// state-only by signature. Because the outcome is `f(shared)` and shared
    /// state is hash-agreed at the final step, every party computes the identical
    /// outcome by construction, and the Host can derive it from the final shared
    /// state. When a handler returns [`Transition::End`], the
    /// generated dispatch glue calls this, serializes the result, and emits
    /// [`Effect::SessionEnd`](crate::Effect::SessionEnd).
    fn outcome(shared: &Self::Shared) -> Self::Outcome;

    /// Select the sole participant allowed to author the next agreed message.
    ///
    /// This is a pure function of replicated state. Returning `None` closes the
    /// agreed-message boundary until another agreed event changes that state.
    /// The host checks this result before invoking [`Self::on_message`], so
    /// network arrival order cannot select between sibling message results.
    fn writer(shared: &Self::Shared) -> Option<Participant>;

    /// Session boundary handler. It receives the committed participant set as
    /// explicit input and may update both state values or emit effects.
    fn on_session_started(
        _ctx: &mut Context<Self::Shared, Self::Local>,
        _ensemble: &Ensemble,
    ) -> Result<ProgramTransition<Self>, ProgramFault> {
        Ok(Transition::Stay)
    }

    /// Message handler applied at the message's canonical agreed position.
    /// Returns [`ApplyDecision::Accept`] to commit the dispatch result and transition
    /// or [`ApplyDecision::Reject`] to decline it without a trace entry.
    fn on_message(
        _ctx: &mut Context<Self::Shared, Self::Local>,
        _from: Participant,
        _msg: Self::Message,
    ) -> MessageApply<Self> {
        Ok(ApplyDecision::Accept(Transition::Stay))
    }
    /// Handler for a callout answer. An error rejects the answer and restores
    /// both state memories without ending the session. A local handler cannot
    /// change agreed shared state or end the session; it queues a message
    /// whose agreed handler does that.
    fn on_input(
        _ctx: &mut LocalContext<Self::Shared, Self::Local>,
        _input: Self::Input,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// Handler for one fired timer. Like [`Self::on_input`], a local handler
    /// cannot change agreed shared state or end the session.
    fn on_timer(
        _ctx: &mut LocalContext<Self::Shared, Self::Local>,
        _timer: TimerPayload,
    ) -> Result<(), ProgramFault> {
        Ok(())
    }

    /// Derive the single open callout implied by the current state.
    ///
    /// The Host calls this after every accepted dispatch, before the resulting
    /// state image is recorded. Returning the same request (index and context)
    /// keeps the current open callout and its `PendingId` after a non-answer
    /// event. An accepted answer consumes its ID, so even an identical next
    /// question receives a new ID. Returning a
    /// different request replaces it with a new one; returning `None`
    /// withdraws it. A terminal transition has no callout regardless of this
    /// result. The callout is computed from state only; it must not depend on
    /// the dispatch that produced the state.
    fn callout(_ctx: &CalloutContext<Self::Shared, Self::Local>) -> Option<Self::Callout> {
        None
    }

    #[doc(hidden)]
    fn __phase(_shared: &Self::Shared) -> Option<Self::Phase> {
        None
    }

    #[doc(hidden)]
    fn __set_phase(_shared: &mut Self::Shared, _phase: Self::Phase) {
        panic!("program is phaseless and cannot apply Transition::To")
    }

    #[doc(hidden)]
    fn __phase_decls() -> &'static [PhaseDecl] {
        &[]
    }

    #[doc(hidden)]
    fn __primitive_routes() -> Vec<PrimitiveRouteSchema> {
        <Self::Shared as SharedState>::__primitive_routes()
    }
}

/// Read-only query surface separated from transition handlers.
pub trait ProgramQuery: Program {
    type Query: Arena0Query;

    /// Evaluate a query against a shared-state snapshot and the committed
    /// participant set supplied for this session.
    fn query(
        shared: &Self::Shared,
        ensemble: &Ensemble,
        query: Self::Query,
    ) -> <Self::Query as Arena0Query>::Response;
}

/// Read-only terminal view surface separated from transition handlers.
pub trait ProgramView: Program {
    /// Render a projection from replicated state, the committed participant
    /// set, and the requested viewport. The ensemble is explicit because it
    /// is session input rather than replicated program state; views remain
    /// pure and cannot access local state or emit effects.
    fn view(shared: &Self::Shared, ensemble: &Ensemble, viewport: &Viewport) -> View;
}
