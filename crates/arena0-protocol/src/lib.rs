//! Protocol-domain types for arena0.
//!
//! Defines protocol identities, events, effects, traces and step attestations,
//! and protocol-facing views. Guest ABI and program artifact contracts belong
//! to `arena0-program`; persistence and wire representations belong to their
//! boundary crates. All protocol values that cross a durable boundary derive
//! Borsh for deterministic binary encoding and Serde for JSON.
//! Process-local system events are non-Borsh structured tracing values.
//!
//! Ids live with their domain (`peer::Id`, `session::Id`, `state::Hash`, ...) and
//! are re-exported here under flat names (`PeerId`, `SessionHash`, `StateHash`).

mod bounded;
mod id;

pub mod admission;
pub mod effect;
pub mod event;
pub mod exec;
pub mod exec_frame;
pub mod execution;
pub mod fetch_frame;
pub mod message;
pub mod negotiation;
pub mod outcome;
pub mod peer;
pub mod session;
pub mod state;
pub mod system_event;
pub mod terminal;
pub mod timer;
pub mod topic;
pub mod trace;
pub mod view;

// Flat id aliases for convenience; the qualified forms (`peer::Id`, ...) remain.
pub use exec::Id as ExecId;
pub use message::Id as MessageId;
pub use negotiation::Id as NegotiationId;
pub use outcome::Hash as OutcomeHash;
pub use peer::Id as PeerId;
pub use peer::IdSource as PeerIdSource;
pub use session::Hash as SessionHash;

pub use admission::ExecutionAdmission;
pub use arena0_program::{
    ABI_VERSION, LocalStateBytes, MAX_LOCAL_STATE_BYTES, MAX_SHARED_STATE_BYTES, ProgramHash,
    SharedStateBytes, StateBytesError,
};
pub use state::Hash as StateHash;
pub use topic::Hash as TopicHash;

pub use effect::{
    DisconnectReason, Effect, EffectClassError, LogLevel, PrivateEffect, PublicEffect,
};
pub use event::{Event, EventClassError, PrivateEvent, PublicEvent};
pub use exec::ExecLifecycle;
pub use exec_frame::{ExecFrame, ExecFrameError};
pub use execution::{
    ABORT_OCCURRENCE_DOMAIN, ABORT_OCCURRENCE_VERSION, AbortKind, AbortOccurrence, BroadcastFrame,
    CommitPlan, DurableEffect, ExecutionBinding, ExecutionInput, ExecutionState, ExecutionStatus,
    ExecutionVersion, FrameId, MAX_ACTIVE_TIMERS, MAX_COMMIT_PLAN_BYTES, MAX_EFFECT_PAYLOAD_BYTES,
    MAX_EXECUTION_INPUT_BYTES, MAX_EXECUTION_STATE_BYTES, MAX_OUTBOX_OCCURRENCES,
    MAX_PRIVATE_EFFECTS, MAX_PRIVATE_RECORD_BYTES, MAX_PROOF_SIGNATURES, MAX_RECEIPT_BYTES,
    MAX_SHARED_EFFECTS, MAX_TERMINAL_OUTCOME_BYTES, MAX_TERMINAL_REASON_BYTES, MAX_TIMER_MUTATIONS,
    MAX_TIMER_PAYLOAD_BYTES, MAX_TRACE_ENTRY_BYTES, OutboxId, OutboxIntent,
    ParticipantStepSignature, ParticipantTerminalSignature, PendingId, PendingIdParseError, PlanId,
    PrivateCause, PrivateCommit, PrivateCursor, PrivateDelta, ProtocolError, PublicCursor,
    PublishedProof, Receipt, ReceiptArtifact, ReceiptBody, ReceiptId, ReceiptKind, ReceiptWork,
    SharedCommit, SharedDelta, SharedProposal, StepCertificate, StopCause, StopReport,
    TerminalCertificate, TerminalOutcome, TerminalProof, TerminalPublication, TimerFiring, TimerId,
    TimerMutation, TransitionOutcome, pending_id, transition,
};
pub use fetch_frame::{FetchFrame, FetchFrameError};
pub use id::IdParseError;
pub use negotiation::{
    ACTIVATION_DOMAIN, ACTIVATION_SIG_DOMAIN, ACTIVATION_SIG_VERSION, ACTIVATION_VERSION,
    ActivationAnnouncement, ActivationData, ActivationSignature, ActivationTickets,
    COUNTEROFFER_DOMAIN, COUNTEROFFER_VERSION, Counteroffer, CounterofferData, CounterofferHash,
    FetchActivationTickets, MAX_NEGOTIATION_FACT_BYTES, MAX_PARAMS_LEN, NEGOTIATION_GOSSIP_DOMAIN,
    NEGOTIATION_GOSSIP_VERSION, NegotiationError, NegotiationFact, NegotiationGossip, OFFER_DOMAIN,
    OFFER_VERSION, Offer, OfferData, OfferHash, TICKET_DOMAIN, TICKET_VERSION, Ticket,
    TicketAction, TicketData, TicketHash, TicketVerificationError,
};
pub use negotiation::{
    Activation, ActivationError, MAX_CLOCK_SKEW_MS, MAX_PARTICIPANTS, MAX_TICKET_LIFETIME_MS,
    PREPARE_WINDOW_MS, PreparedActivation,
};
pub use outcome::{SessionTermination, TrapKind};
pub use session::{Committed, Ensemble, EnsembleError, Lifecycle, Nonce, Open, Participant};
pub use system_event::{
    EventSource, ExecCreationOrigin, ExecutionEvent, ExecutionFailureCode, NegotiationEvent,
    NegotiationStage, SystemEvent, TerminalKind,
};
pub use terminal::TerminalResult;
pub use timer::{TimerPayload, TimerSpec};
pub use trace::{
    AggregateAttestation, AttestationError, CHAIN_START, DivergenceDiagnostic, DivergenceKind,
    PendingKind, PendingOperation, PendingRecord, PrivateRecord, ReceiptTermination,
    STEP_COMMIT_DOMAIN, SessionHeader, SessionTerminal, SignerSet, StepCommitment, StepSig,
    TERMINAL_DOMAIN, TRACE_FORMAT_VERSION, TerminalCommitment, TraceEntry, WitnessCommitment,
};
pub use view::{ColorDepth, Slot, View, Viewport};
