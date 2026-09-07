//! Shared support types for session negotiation.
//!
//! The negotiation driver lives in [`super::driver`]. This module keeps the
//! small callback and error contracts that the runtime and daemon share,
//! plus the fetch helper used to serve frozen evidence.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use arena0_protocol::{
    Activation, ActivationTickets, EventSource, ExecId, FetchActivationTickets, FetchFrame,
    NegotiationEvent, Offer, PreparedActivation, SessionHash, StateHash, Ticket,
};
use arena0_store::ExecutionStore;
use arena0_transport::{NegotiationTopic, RecvHandle, Transport};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, timeout_at};

/// Result of the first durable prepare attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareOutcome {
    /// The activation facts were inserted or already existed exactly.
    Accepted,
    /// The activation cannot be prepared at the current wall-clock time.
    NotPreparable(String),
    /// This local execution is already bound to different preparation facts.
    Conflict,
}

/// Whether a durable commit compare-and-set accepted the activation facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurableOutcome {
    /// The facts were inserted or already existed exactly.
    Accepted,
    /// This local execution was already bound to different activation facts.
    Conflict,
}

/// A signed local withdrawal submitted while negotiation is still revocable.
#[derive(Debug)]
pub struct LocalTicketWithdrawal {
    pub ticket: Ticket,
    pub accepted: tokio::sync::oneshot::Sender<bool>,
}

/// One-shot convergence-fetch response timeout (requester and serving side).
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// How one negotiation attempt begins.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum NegotiationStart {
    /// Negotiate a new offer, countering with the preferred params when they
    /// differ.
    Fresh {
        /// The validated offer with the creator's ticket hash already present.
        offer: Offer,
        /// The creator's signed ticket matching the first hash in `offer`.
        /// A participant may receive this together with its selected offer,
        /// avoiding a second ordering race; gossip remains the retry path.
        creator_ticket: Option<Ticket>,
        preferred_params: Option<Vec<u8>>,
    },
    /// Restore the exact durable activation evidence and local ticket.
    Resume {
        local_ticket: Ticket,
        activation: Box<PreparedActivation>,
    },
}

/// Agent-facing channels installed together for a supervised negotiation.
#[allow(missing_debug_implementations)]
pub struct NegotiationSupervision {
    pub(crate) withdrawals: mpsc::Receiver<LocalTicketWithdrawal>,
    pub(crate) withdrawal_requested: watch::Receiver<bool>,
    pub(crate) ticket: watch::Sender<Option<Ticket>>,
    pub(crate) offer: watch::Sender<Offer>,
}

impl NegotiationSupervision {
    #[must_use]
    pub fn new(
        withdrawals: mpsc::Receiver<LocalTicketWithdrawal>,
        withdrawal_requested: watch::Receiver<bool>,
        ticket: watch::Sender<Option<Ticket>>,
        offer: watch::Sender<Offer>,
    ) -> Self {
        Self {
            withdrawals,
            withdrawal_requested,
            ticket,
            offer,
        }
    }
}

/// All domain inputs for one negotiation attempt.
#[allow(missing_debug_implementations)]
pub struct NegotiationAttempt {
    pub topic: Box<dyn NegotiationTopic>,
    pub exec_id: ExecId,
    pub start: NegotiationStart,
    pub supervision: Option<NegotiationSupervision>,
    /// The outer deadline, when the caller wants bounded negotiation. `None`
    /// keeps discovery open until the offer reaches the prepared boundary;
    /// the driver then installs its bounded completion deadline.
    pub deadline: Option<Instant>,
}

/// Async callback used at the durable prepared-activation boundary.
///
/// The execution writer is supplied by the Host for every invocation. This
/// keeps the non-Clone capability in the negotiation-to-actor handoff while
/// still allowing the surrounding daemon to attach its own lifecycle
/// projections.
pub type PrepareEffect = Box<
    dyn for<'a> FnMut(
            &'a mut ExecutionStore,
            PreparedActivation,
        )
            -> Pin<Box<dyn Future<Output = Result<PrepareOutcome, String>> + Send + 'a>>
        + Send,
>;

/// Async callback used at the durable committed-activation boundary.
pub type PersistActivationEffect = Box<
    dyn for<'a> FnMut(
            &'a mut ExecutionStore,
            Activation,
        )
            -> Pin<Box<dyn Future<Output = Result<DurableOutcome, String>> + Send + 'a>>
        + Send,
>;

/// Synchronous callback used to recompute the creator's initial state when it
/// evaluates a counteroffer.
pub type RecomputeInitialStateEffect = Box<dyn FnMut(&[u8]) -> Result<StateHash, String> + Send>;

/// Effects requested by a negotiation attempt at its owner boundaries.
#[allow(missing_debug_implementations)]
pub struct NegotiationEffects<'a> {
    pub prepare: PrepareEffect,
    pub persist_activation: PersistActivationEffect,
    pub recompute_initial_state: RecomputeInitialStateEffect,
    pub emit: &'a (dyn Fn(EventSource, NegotiationEvent) + Send + Sync),
}

/// Why the local negotiation drive could not safely finish.
#[derive(Debug, thiserror::Error)]
pub enum NegotiationDriveError {
    /// The lifecycle writer was issued by another Host instance.
    #[error("execution store belongs to another host")]
    ExecutionStoreHostMismatch,
    /// The lifecycle writer was issued for a different execution identity.
    #[error("execution store is bound to {store}, requested execution {requested}")]
    ExecutionStoreMismatch { store: ExecId, requested: ExecId },
    /// The local ticket was malformed, invalid, expired, or not bound to the offer.
    #[error("invalid local negotiation ticket")]
    InvalidLocalTicket,
    /// The creator froze an exact participant set that excludes this Host.
    #[error("creator selected a different participant set")]
    NotSelected,
    /// The offer names a deterministic execution environment this Host cannot run.
    #[error("execution profile mismatch: offered {offered}, local {local}")]
    ExecutionProfileMismatch {
        offered: arena0_program::ExecutionProfileHash,
        local: arena0_program::ExecutionProfileHash,
    },
    /// Durable state belongs to another negotiation.
    #[error("prepared activation belongs to another negotiation")]
    InvalidPreparedState,
    /// Signing a negotiation fact failed.
    #[error("activation signing failed: {0}")]
    Signing(String),
    /// Durable preparation failed before the activation signature could be emitted.
    #[error("durable activation preparation failed: {0}")]
    Prepare(String),
    /// Persisting the complete activation failed.
    #[error("persisting activation failed: {0}")]
    PersistCommit(String),
    /// The inbound fetch-stream router stopped.
    #[error("fetch stream router stopped")]
    StreamClosed,
    /// The deadline elapsed before this execution prepared any proposal.
    #[error("negotiation timed out before activation preparation")]
    Timeout,
    /// The deadline elapsed after durable preparation, so activation may have
    /// completed elsewhere and the execution must not be reused.
    #[error("negotiation outcome is unknown after durable activation preparation")]
    UnknownOutcome,
    /// The local ticket was withdrawn: negotiation is over, so the drive must
    /// stop holding the daemon's negotiation slot.
    #[error("negotiation withdrawn locally")]
    Withdrawn,
}

/// Current Unix time in milliseconds, clamped to `u64::MAX` on clock skew.
pub fn unix_time_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

/// Serve one convergence-fetch request from frozen evidence: the authenticated
/// requester may be any peer (the evidence is public broadcast material); the
/// response is the exact frozen ticket set, one bounded response. The request
/// frame was already consumed by the accept router (which routed by its
/// session hash); the handler re-checks the hash before responding.
pub async fn serve_fetch_evidence(
    transport: &std::sync::Arc<dyn Transport + Sync>,
    recv: &RecvHandle,
    request: FetchActivationTickets,
    session_hash: SessionHash,
    tickets: &[Ticket],
    deadline: Instant,
) {
    if request.session_hash != session_hash {
        return;
    }
    let requester = *recv.remote_peer();
    let response = FetchFrame::ActivationTickets(ActivationTickets {
        session_hash: request.session_hash,
        tickets: tickets.to_vec(),
    });
    let Ok(Ok(send)) = timeout_at(deadline, transport.open_fetch(&requester)).await else {
        return;
    };
    let _ = timeout_at(deadline, send.send_fetch(&response)).await;
}
