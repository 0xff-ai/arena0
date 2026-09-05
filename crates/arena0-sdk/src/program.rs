//! The [`Program`] trait that every arena0 program implements.

use arena0_protocol::TimerPayload;
use arena0_protocol::{Ensemble, Participant, View, Viewport};
use borsh::BorshDeserialize;
use core::convert::Infallible;

use crate::{
    Arena0Callout, Arena0Phase, Arena0Query, Context, InputFault, LocalState, PhaseDecl,
    PhasedSharedState, PrimitiveRouteSchema, ProgramFault, ProtocolFault, SharedContext,
    SharedState, Transition,
};

pub type ProgramTransition<P> = Transition<<P as Program>::Phase>;

/// The outcome of a shared message apply: accept (with a transition) or reject.
///
/// `Reject` is a deterministic non-application: the host rolls the candidate
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
/// Shared handlers receive [`SharedContext`], while local handlers receive
/// `Context<Self::Shared, Self::Local>`. Handlers come in two classes:
///
/// - **Shared-event handlers** ([`on_session_started`](Self::on_session_started),
///   [`on_message`](Self::on_message)) run identically on every node at the
///   same public trace position, as pure functions of (shared state, event).
///   They may mutate shared state; entropy, callouts, timers, broadcasts, and
///   local-state access are unavailable in the type system.
/// - **Local handlers** ([`on_react`](Self::on_react),
///   [`on_input`](Self::on_input), timers, sign results) see a read-only
///   shared view plus private state, may draw entropy, request callouts, and
///   emit broadcasts. They never mutate shared state: a node's decision
///   reaches shared state only through the message it broadcasts, which every
///   node (the sender included) applies through the same shared handler.
///
/// Module-shell programs may await arena-owned callouts inside async handlers.
/// The program macro lowers `ctx.effects().callout(...).await?` into generated
/// continuation state, and the callout answer resumes through the `on_input`
/// dispatch path. Validation after such an await reports [`InputFault`] even if
/// the source handler's visible signature returns [`ProgramFault`]; use
/// `.retryable()?` when the answer should be retried.
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
    type Callout: Arena0Callout<Response = Self::Input>;
    type Input: borsh::BorshSerialize + BorshDeserialize + crate::ProgramValue + 'static;
    type Params: serde::de::DeserializeOwned + serde::Serialize + crate::ProgramValue;
    type Outcome: crate::ProgramValue
        + serde::Serialize
        + borsh::BorshSerialize
        + borsh::BorshDeserialize
        + 'static;

    fn initialize(
        _ctx: &mut SharedContext<Self::Shared>,
        _params: Self::Params,
    ) -> Result<(), ProgramFault> {
        Ok(())
    }

    /// Derive the typed terminal outcome from final shared state.
    ///
    /// This is a pure projection: no [`Context`], no effects, no randomness, and
    /// shared-only by signature. Because the outcome is `f(shared)` and shared
    /// state is hash-agreed at the final step, every party computes the identical
    /// outcome by construction, and a replaying verifier recomputes it from the
    /// same wasm and final state. When a handler returns [`Transition::End`], the
    /// generated dispatch glue calls this, serializes the result, and emits
    /// [`Effect::SessionEnd`](crate::Effect::SessionEnd).
    fn outcome(shared: &Self::Shared) -> Self::Outcome;

    /// Select the sole participant allowed to author the next public message.
    ///
    /// This is a pure function of replicated state. Returning `None` closes the
    /// public-message boundary until another public event changes that state.
    /// The host checks this result before invoking [`Self::on_message`], so
    /// network arrival order cannot select between sibling candidates.
    fn writer(shared: &Self::Shared) -> Option<Participant>;

    /// Shared boundary handler: applied by every node at public position 0.
    /// A pure function of (shared state, ensemble); typically sizes shared
    /// buffers and moves to the initial phase.
    fn on_session_started(
        _ctx: &mut SharedContext<Self::Shared>,
        _ensemble: &Ensemble,
    ) -> Result<ProgramTransition<Self>, ProgramFault> {
        Ok(Transition::Stay)
    }

    /// Local decision hook, run by the node's own runtime after every applied
    /// public entry (the session-start boundary included) unless a callout is
    /// already pending. Reads shared state, writes private state, draws
    /// entropy, requests callouts, and emits broadcasts; recorded only in the
    /// node's private trace section.
    fn on_react(_ctx: &mut Context<Self::Shared, Self::Local>) -> Result<(), ProgramFault> {
        Ok(())
    }

    /// Shared message handler: applied by every node at the message's canonical
    /// public position, as a pure function of (shared state, message).
    ///
    /// Returns [`ApplyDecision::Accept`] to apply the message (with an optional
    /// transition) or [`ApplyDecision::Reject`] to deterministically decline it:
    /// a rejected apply leaves no trace entry and the host quarantines the
    /// message. The shared context has no local-state, entropy, effects, or
    /// node-identity access. Local state is not present in this boundary; the
    /// host restores only the shared bytes when a candidate is rejected.
    fn on_message(
        _ctx: &mut SharedContext<Self::Shared>,
        _from: Participant,
        _msg: Self::Message,
    ) -> MessageApply<Self> {
        Ok(ApplyDecision::Accept(Transition::Stay))
    }
    /// Local handler for a callout answer. Emits broadcasts and writes private
    /// state; shared transitions come only from the shared handlers that apply
    /// the resulting messages.
    fn on_input(
        _ctx: &mut Context<Self::Shared, Self::Local>,
        _input: Self::Input,
    ) -> Result<(), InputFault> {
        Ok(())
    }
    #[doc(hidden)]
    fn __arena0_on_signed(
        _ctx: &mut Context<Self::Shared, Self::Local>,
        _signature: Vec<u8>,
    ) -> Result<(), ProgramFault> {
        Ok(())
    }

    #[doc(hidden)]
    fn __arena0_restore_continuation(_ctx: &mut Context<Self::Shared, Self::Local>, _tag: u32) {}
    fn on_timer(_ctx: &mut Context<Self::Shared, Self::Local>) -> Result<(), ProgramFault> {
        Ok(())
    }
    #[doc(hidden)]
    fn __arena0_on_typed_timer(
        ctx: &mut Context<Self::Shared, Self::Local>,
        _timer: TimerPayload,
    ) -> Result<(), ProgramFault> {
        Self::on_timer(ctx)
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

/// Phase-aware extension for programs whose shared state carries a managed phase.
pub trait PhasedProgram: Program {
    fn phase(shared: &Self::Shared) -> Self::Phase;

    #[doc(hidden)]
    fn __set_managed_phase(shared: &mut Self::Shared, phase: Self::Phase);
}

impl<P> PhasedProgram for P
where
    P: Program,
    P::Shared: PhasedSharedState<Phase = P::Phase>,
{
    fn phase(shared: &Self::Shared) -> Self::Phase {
        P::Shared::phase(shared)
    }

    fn __set_managed_phase(shared: &mut Self::Shared, phase: Self::Phase) {
        P::Shared::__set_phase(shared, phase);
    }
}

/// A genuinely phaseless transition type for lower-level `Program` canaries.
pub type PhaselessTransition = Transition<Infallible>;

/// Read-only query surface separated from transition handlers.
pub trait ProgramQuery: Program {
    type Query: Arena0Query;

    fn query(
        ctx: &SharedContext<Self::Shared>,
        query: Self::Query,
    ) -> <Self::Query as Arena0Query>::Response;
}

/// Read-only terminal view surface separated from transition handlers.
pub trait ProgramView: Program {
    fn view(ctx: &SharedContext<Self::Shared>, viewport: &Viewport) -> View;
}
