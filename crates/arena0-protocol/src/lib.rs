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

pub mod blob;
mod id;
pub mod limits;
pub use blob::{Attachment, BlobError, BlobHandle, BlobHash, RangeAttachment};
pub use limits::{
    MAX_BLOB_BYTES, MAX_DIRECT_CONTROL_BYTES, MAX_DIRECT_RANGE_BYTES, MAX_DIRECT_SLICE_BYTES,
    MAX_EFFECT_PAYLOAD_BYTES, MAX_PARTICIPANTS, MAX_TERMINAL_OUTCOME_BYTES,
    MAX_TERMINAL_REASON_BYTES, MAX_TIMER_PAYLOAD_BYTES,
};

#[cfg(not(target_arch = "wasm32"))]
pub mod admission;
pub mod effect;
pub mod event;
#[cfg(not(target_arch = "wasm32"))]
pub mod exec;
#[cfg(not(target_arch = "wasm32"))]
pub mod exec_frame;
#[cfg(not(target_arch = "wasm32"))]
pub mod execution;
#[cfg(not(target_arch = "wasm32"))]
pub mod fetch_frame;
pub mod message;
#[cfg(not(target_arch = "wasm32"))]
pub mod negotiation;
pub mod outcome;
pub mod peer;
pub mod session;
pub mod state;
#[cfg(not(target_arch = "wasm32"))]
pub mod system_event;
pub mod timer;
pub mod topic;
#[cfg(not(target_arch = "wasm32"))]
pub mod trace;
pub mod verify;
pub mod view;

// Flat id aliases for convenience; the qualified forms (`peer::Id`, ...) remain.
#[cfg(not(target_arch = "wasm32"))]
pub use exec::Id as ExecId;
pub use message::Id as MessageId;
#[cfg(not(target_arch = "wasm32"))]
pub use negotiation::Id as NegotiationId;
pub use peer::Id as PeerId;
pub use peer::IdSource as PeerIdSource;
pub use session::Hash as SessionHash;

#[cfg(not(target_arch = "wasm32"))]
pub use admission::{ExecutionAdmission, NegotiationTarget};
pub use arena0_program::{
    ABI_VERSION, LocalStateBytes, MAX_LOCAL_STATE_BYTES, MAX_SHARED_STATE_BYTES, ProgramHash,
    SharedStateBytes, StateBytesError,
};
pub use state::Hash as StateHash;
pub use topic::Hash as TopicHash;

pub use effect::{Effect, EffectKind, EffectSummary, LogLevel};
pub use event::{Event, EventKind};
#[cfg(not(target_arch = "wasm32"))]
pub use exec::ExecLifecycle;
#[cfg(not(target_arch = "wasm32"))]
pub use exec_frame::{ExecFrame, MAX_EXEC_FRAME_BYTES};
#[cfg(not(target_arch = "wasm32"))]
pub use execution::{
    ABORT_OCCURRENCE_DOMAIN, ABORT_OCCURRENCE_VERSION, AbortKind, AbortOccurrence, CalloutId,
    CalloutIdParseError, EndMatch, EndPhase, ExecutionBinding, ExecutionState, ExecutionStatus,
    ExecutionVersion, MAX_ACTIVE_TIMERS, MAX_EFFECTS, MAX_EXECUTION_STATE_BYTES,
    MAX_PROOF_SIGNATURES, MAX_RECEIPT_BYTES, MAX_TRACE_ENTRY_BYTES, OpenCallout,
    ParticipantStepSignature, ProtocolError, Receipt, ReceiptArtifact, ReceiptBody, ReceiptId,
    ReceiptKind, ReceiptProvenance, ReceiptSummary, ReceiptWork, SharedProposal, StepCertificate,
    StepCursor, StopCause, StopReport, TerminalOutcome, TimerId, callout_id, validate_agreed_trace,
};
#[cfg(not(target_arch = "wasm32"))]
pub use fetch_frame::FetchFrame;
pub use id::IdParseError;
#[cfg(not(target_arch = "wasm32"))]
pub use negotiation::{
    ACTIVATION_DOMAIN, ACTIVATION_SIG_DOMAIN, ACTIVATION_SIG_VERSION, ACTIVATION_VERSION,
    ActivationAnnouncement, ActivationData, ActivationSignature, ActivationTickets,
    COUNTEROFFER_DOMAIN, COUNTEROFFER_VERSION, Counteroffer, CounterofferData, CounterofferHash,
    FetchActivationTickets, MAX_NEGOTIATION_FACT_BYTES, MAX_PARAMS_LEN, NEGOTIATION_GOSSIP_DOMAIN,
    NEGOTIATION_GOSSIP_VERSION, NegotiationError, NegotiationFact, NegotiationGossip, OFFER_DOMAIN,
    OFFER_VERSION, Offer, OfferData, OfferHash, TICKET_DOMAIN, TICKET_VERSION, Ticket,
    TicketAction, TicketData, TicketHash, TicketVerificationError,
};
#[cfg(not(target_arch = "wasm32"))]
pub use negotiation::{
    Activation, ActivationError, MAX_CLOCK_SKEW_MS, MAX_TICKET_LIFETIME_MS, PREPARE_WINDOW_MS,
    PreparedActivation,
};
pub use outcome::SessionTermination;
pub use session::{Committed, Ensemble, EnsembleError, Nonce, Open, Participant};
#[cfg(not(target_arch = "wasm32"))]
pub use system_event::{
    EventSource, ExecCreationOrigin, ExecutionEvent, ExecutionFailureCode, NegotiationEvent,
    NegotiationStage, SystemEvent, TerminalKind,
};
pub use timer::TimerPayload;
#[cfg(not(target_arch = "wasm32"))]
pub use trace::{
    AggregateAttestation, AttestationError, CHAIN_START, ReceiptTermination, STEP_COMMIT_DOMAIN,
    SessionHeader, SignerSet, StepCommitment, StepEvent, StepSig, StepTerminal,
    TRACE_FORMAT_VERSION, TraceEntry,
};
pub use verify::VerifyError;
pub use view::{ColorDepth, Slot, View, Viewport};
