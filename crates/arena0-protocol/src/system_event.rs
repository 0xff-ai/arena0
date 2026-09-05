//! Redacted process-local host events.
//!
//! These types describe host and protocol facts. They are not guest ABI,
//! guest wire, receipt, or durable trace types.

use crate::PendingId;
use valuable::Valuable;

use crate::{ExecId, ExecLifecycle, NegotiationId, PeerId, SessionHash, StateHash, TicketHash};
use arena0_program::ProgramHash;

/// One safe host or protocol occurrence for structured tracing.
#[derive(Debug, Clone, PartialEq, Eq, Valuable, serde::Serialize, serde::Deserialize)]
pub enum SystemEvent {
    Negotiation {
        source: EventSource,
        event: NegotiationEvent,
    },
    Execution {
        source: EventSource,
        event: ExecutionEvent,
    },
}

impl SystemEvent {
    /// Identity context for this occurrence.
    #[must_use]
    pub const fn source(&self) -> &EventSource {
        match self {
            Self::Negotiation { source, .. } | Self::Execution { source, .. } => source,
        }
    }
}

/// Typed identity context carried across tasks and threads.
#[derive(Debug, Clone, PartialEq, Eq, Valuable, serde::Serialize, serde::Deserialize)]
pub enum EventSource {
    Negotiation {
        peer_id: PeerId,
        exec_id: ExecId,
        program_id: ProgramHash,
        negotiation_id: NegotiationId,
    },
    Execution {
        peer_id: PeerId,
        exec_id: ExecId,
        program_id: ProgramHash,
    },
    Session {
        peer_id: PeerId,
        exec_id: ExecId,
        program_id: ProgramHash,
        session_hash: SessionHash,
    },
}

/// Safe negotiation progress and decisions.
#[derive(Debug, Clone, PartialEq, Eq, Valuable, serde::Serialize, serde::Deserialize)]
pub enum NegotiationEvent {
    PeersChanged {
        lifecycle: ExecLifecycle,
        peers: Vec<PeerId>,
    },
    Started {
        target_size: u16,
    },
    OfferAccepted {
        creator: PeerId,
        offer_seq: u64,
    },
    TicketAccepted {
        participant: PeerId,
        ticket_hash: TicketHash,
        ticket_count: u16,
        target_size: u16,
    },
    ActivationPrepared {
        session_hash: SessionHash,
        participant_count: u16,
    },
    PreparedActivationResumed {
        session_hash: SessionHash,
        participant_count: u16,
    },
    ActivationCommitted {
        session_hash: SessionHash,
        participant_count: u16,
    },
    Retry {
        attempt: u64,
        stage: NegotiationStage,
        ticket_count: u16,
        sig_count: u16,
        target_size: u16,
    },
    TopicRejoined,
    TimedOut {
        stage: NegotiationStage,
        ticket_count: u16,
        sig_count: u16,
        target_size: u16,
    },
}

/// Private negotiation progress projected without signed or opaque payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Valuable, serde::Serialize, serde::Deserialize)]
pub enum NegotiationStage {
    Gossiping,
    Prepared,
}

/// Why an execution record was created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Valuable, serde::Serialize, serde::Deserialize)]
pub enum ExecCreationOrigin {
    Request,
    Recovery,
}

/// Safe execution facts. Agent-facing JSON remains in the local API frame.
#[derive(Debug, Clone, PartialEq, Eq, Valuable, serde::Serialize, serde::Deserialize)]
pub enum ExecutionEvent {
    Created {
        origin: ExecCreationOrigin,
    },
    SessionStarted {
        ensemble: Vec<PeerId>,
    },
    StepCommitted {
        step: u64,
        pre_state: StateHash,
        post_state: StateHash,
        fuel_used: u64,
        signer_count: u16,
        participant_count: u16,
    },
    CalloutRequested {
        pending_id: PendingId,
        callout_index: u32,
    },
    CalloutAnswered {
        pending_id: PendingId,
    },
    Terminal {
        kind: TerminalKind,
    },
}

/// Terminal class without a private reason or opaque outcome.
#[derive(Debug, Clone, PartialEq, Eq, Valuable, serde::Serialize, serde::Deserialize)]
pub enum TerminalKind {
    Completed,
    Aborted {
        step: u64,
        failure: ExecutionFailureCode,
    },
    Failed {
        failure: ExecutionFailureCode,
    },
}

/// Closed failure classes for execution failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Valuable, serde::Serialize, serde::Deserialize)]
pub enum ExecutionFailureCode {
    Negotiation,
    HostStopped,
    ProgramAborted,
    Runtime,
    InvalidGuestOutput,
}
