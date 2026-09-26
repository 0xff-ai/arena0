//! Redacted process-local host events.
//!
//! These types describe host and protocol facts. They are not guest ABI,
//! guest wire, receipt, or durable trace types.

use crate::CalloutId;
use valuable::Valuable;

use crate::{ExecId, ExecLifecycle, NegotiationId, PeerId, SessionHash, TicketHash};
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

impl EventSource {
    /// The most specific source for an execution: its session once the
    /// execution aggregate exists, else its negotiation, else the bare
    /// execution (an open Join can fail before it accepts an offer, so no
    /// negotiation identity exists yet).
    #[must_use]
    pub const fn most_specific(
        peer_id: PeerId,
        exec_id: ExecId,
        program_id: ProgramHash,
        negotiation_id: Option<NegotiationId>,
        session_hash: Option<SessionHash>,
    ) -> Self {
        match (session_hash, negotiation_id) {
            (Some(session_hash), _) => Self::Session {
                peer_id,
                exec_id,
                program_id,
                session_hash,
            },
            (None, Some(negotiation_id)) => Self::Negotiation {
                peer_id,
                exec_id,
                program_id,
                negotiation_id,
            },
            (None, None) => Self::Execution {
                peer_id,
                exec_id,
                program_id,
            },
        }
    }
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
    CalloutRequested {
        pending_id: CalloutId,
        callout_index: u32,
    },
    CalloutAnswered {
        pending_id: CalloutId,
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
