use crate::exec::ExecLifecycle;
use crate::{ActivationError, PeerId, StateHash};

use super::{OutboxId, TimerId};
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    /// An input is not legal for the public lifecycle.
    #[error("input {input} is illegal while execution is {current:?}")]
    IllegalLifecycle {
        /// Current public lifecycle projection.
        current: ExecLifecycle,
        /// Stable input name.
        input: &'static str,
    },
    /// Activation validation failed.
    #[error("invalid activation: {0}")]
    InvalidActivation(ActivationError),
    /// Immutable activation copies disagree.
    #[error("execution binding does not match its activation")]
    BindingMismatch,
    /// A participant was not in the activation.
    #[error("unknown participant {participant}")]
    UnknownParticipant { participant: PeerId },
    /// Version overflow prevents another durable plan.
    #[error("execution version exhausted")]
    VersionExhausted,
    /// Public step overflow prevents another public commit.
    #[error("public cursor exhausted")]
    PublicCursorExhausted,
    /// Private sequence overflow prevents another private record.
    #[error("private cursor exhausted")]
    PrivateCursorExhausted,
    /// A private delta did not use the next local record sequence.
    #[error("private record sequence mismatch: expected {expected}, got {actual}")]
    InvalidPrivateSequence { expected: u64, actual: u64 },
    /// The automatic local reaction at a public position is already durable.
    #[error("private reaction at public position {position} is already committed")]
    PrivateReactionAlreadyCommitted { position: u64 },
    /// A recovered private reaction cursor is inconsistent with the aggregate.
    #[error("private reaction cursor is inconsistent with public/private progress")]
    InvalidPrivateReactionCursor,
    /// The shared proposal already exists.
    #[error("a shared proposal is already pending")]
    SharedProposalExists,
    /// A shared signature arrived without a proposal.
    #[error("no shared proposal is pending")]
    SharedProposalMissing,
    /// The proposed entry position does not match the public cursor.
    #[error("step coordinate mismatch: expected {expected}, got {actual}")]
    StepCoordinateMismatch { expected: u64, actual: u64 },
    /// The proposed entry pre-state does not match the public cursor.
    #[error("public pre-state mismatch")]
    PublicPreStateMismatch {
        /// Expected state hash.
        expected: StateHash,
        /// Received state hash.
        actual: StateHash,
    },
    /// The proposed entry link does not match the public cursor.
    #[error("public commitment chain link mismatch")]
    PublicChainMismatch,
    /// The committed shared bytes do not match the entry post-state.
    #[error("shared delta state hash mismatch")]
    PublicStateHashMismatch,
    /// The public entry has the wrong trace format.
    #[error("unsupported trace version {actual}, expected {expected}")]
    TraceVersionMismatch { actual: u32, expected: u32 },
    /// Public step zero must be the exact activation start boundary.
    #[error("public step zero is missing SessionStarted")]
    MissingSessionStart,
    /// A SessionStarted event appeared after public step zero.
    #[error("SessionStarted is only legal at public step zero")]
    SessionStartPosition,
    /// The start event's ensemble differs from the activation participants.
    #[error("SessionStarted ensemble does not match the activation")]
    SessionStartMismatch,
    /// A local-only event was supplied as shared consensus evidence.
    #[error("local event class is not legal in shared evidence")]
    IllegalSharedEvent,
    /// A local-only effect was supplied as shared consensus evidence.
    #[error("local effect class is not legal in shared evidence")]
    IllegalSharedEffect,
    /// The public entry was too large.
    #[error("trace entry is {actual} bytes; maximum is {max}")]
    TraceEntryTooLarge { actual: usize, max: usize },
    /// The private record was too large.
    #[error("private record is {actual} bytes; maximum is {max}")]
    PrivateRecordTooLarge { actual: usize, max: usize },
    /// A bounded collection exceeded its semantic maximum.
    #[error("{kind} has {actual} items; maximum is {max}")]
    CollectionTooLarge {
        /// Collection name.
        kind: &'static str,
        /// Actual item count.
        actual: usize,
        /// Maximum count.
        max: usize,
    },
    /// A signature did not match the pending commitment.
    #[error("signature for {participant} does not match step {step}")]
    InvalidStepSignature { participant: PeerId, step: u64 },
    /// A duplicate participant signature repeated the same content.
    #[error("duplicate step signature from {participant}")]
    DuplicateStepSignature { participant: PeerId },
    /// A duplicate participant signature conflicted with prior content.
    #[error("conflicting step signature from {participant}")]
    ConflictingStepSignature { participant: PeerId },
    /// A terminal signature did not verify.
    #[error("terminal signature from {participant} is invalid")]
    InvalidTerminalSignature { participant: PeerId },
    /// A duplicate terminal signature repeated the same content.
    #[error("duplicate terminal signature from {participant}")]
    DuplicateTerminalSignature { participant: PeerId },
    /// A duplicate terminal signature conflicted with prior content.
    #[error("conflicting terminal signature from {participant}")]
    ConflictingTerminalSignature { participant: PeerId },
    /// A proof did not contain all activation participants.
    #[error("proof has {actual} signatures; expected {expected}")]
    IncompleteProof { actual: usize, expected: usize },
    /// Aggregate construction or verification failed.
    #[error("invalid proof certificate: {0}")]
    InvalidCertificate(String),
    /// Terminal proof is missing.
    #[error("terminal proof is not pending")]
    TerminalProofMissing,
    /// A complete receipt body does not match the durable execution binding.
    #[error("receipt body does not match the execution binding or terminal certificate")]
    ReceiptBodyMismatch,
    /// A guest signing request is malformed or lacks the kernel binding.
    #[error("invalid guest signing request")]
    InvalidGuestSignData,
    /// A signed abort occurrence's identity or bounded content is invalid.
    #[error("invalid execution abort occurrence")]
    InvalidAbortOccurrence,
    /// A signed abort occurrence's Ed25519 signature is malformed.
    #[error("invalid execution abort signature: {0}")]
    InvalidAbortSignature(String),
    /// The abort occurrence did not carry the exact current public head.
    #[error("invalid execution abort public coordinate")]
    InvalidAbortCoordinate,
    /// An abort occurrence is not signed by an activation participant.
    #[error("execution abort occurrence is not activation-authenticated")]
    UnauthenticatedAbort,
    /// A complete receipt trace does not contain exactly one valid terminal edge.
    #[error("receipt trace does not match terminal proof")]
    TerminalTraceMismatch,
    /// The guest's agent-facing JSON outcome was malformed.
    #[error("invalid terminal outcome JSON projection: {0}")]
    InvalidOutcomeProjection(String),
    /// A successful terminal entry did not carry both outcome projections.
    #[error("successful terminal entry requires a complete outcome projection")]
    TerminalOutcomeRequired,
    /// A terminal outcome projection was supplied for a non-successful or
    /// multiply-terminal entry.
    #[error("terminal outcome projection does not match the shared entry")]
    TerminalOutcomeMismatch,
    /// The guest's Borsh outcome differs from the `SessionEnd` effect bytes.
    #[error("terminal outcome Borsh bytes do not match the SessionEnd effect")]
    OutcomeProjectionMismatch,
    /// A private execution event was not paired with its typed cause.
    #[error("private cause does not match the local event")]
    PrivateCauseMismatch,
    /// A local answer or signature did not identify the stored continuation.
    #[error("private continuation does not match the pending continuation")]
    PendingContinuationMismatch,
    /// A persisted continuation id does not derive from its private origin.
    #[error("pending continuation id does not match its private coordinate")]
    PendingCoordinateMismatch,
    /// A private record emitted an invalid continuation shape.
    #[error("private continuation effect is invalid")]
    InvalidPendingContinuation,
    /// A standalone timer firing cannot be committed without its guest result.
    #[error("timer firing must be committed with its private guest result")]
    TimerResultRequired,
    /// Terminal proof is already complete.
    #[error("terminal proof is already published")]
    TerminalAlreadyPublished,
    /// Durable terminal status is missing or inconsistent with its projection.
    #[error("durable terminal status is invalid or inconsistent")]
    InvalidTerminalStatus,
    /// A terminal transition had more than one terminal effect.
    #[error("shared entry contains multiple terminal effects")]
    MultipleTerminalEffects,
    /// A non-terminal shared entry appeared after terminal effect preparation.
    #[error("terminal proof is pending and cannot accept another public step")]
    TerminalProofPending,
    /// The state already has terminal lifecycle.
    #[error("execution is terminal")]
    AlreadyTerminal,
    /// A private context supplied a different number of timer arms than the
    /// corresponding guest record emitted.
    #[error("private timer context has {actual} entries; guest record has {expected}")]
    TimerContextMismatch { expected: usize, actual: usize },
    /// A guest terminal effect cannot be emitted from a private record.
    #[error("private record contains a terminal guest effect")]
    PrivateTerminalEffect,
    /// Phase 1 permits at most one broadcast in a private execution record.
    #[error("private record contains multiple broadcasts")]
    MultipleBroadcasts,
    /// A timer firing did not match an active timer identity.
    #[error("timer firing does not match an active timer")]
    StaleTimerFiring,
    /// A timer deadline overflowed the supplied deterministic base time.
    #[error("timer deadline overflowed")]
    TimerDeadlineOverflow,
    /// A timer mutation was repeated in one plan.
    #[error("duplicate timer mutation for {timer_id:?}")]
    DuplicateTimerMutation { timer_id: TimerId },
    /// Active timer set is not canonical.
    #[error("active timer set is not canonical")]
    TimerSetNotCanonical,
    /// Active timer count exceeded its bound.
    #[error("active timer set has {actual} entries; maximum is {max}")]
    TooManyTimers { actual: usize, max: usize },
    /// Payload exceeded its bound.
    #[error("{kind} is {actual} bytes; maximum is {max}")]
    PayloadTooLarge {
        /// Payload name.
        kind: &'static str,
        /// Actual byte length.
        actual: usize,
        /// Maximum byte length.
        max: usize,
    },
    /// Encoded value exceeded its total bound.
    #[error("{kind} is {actual} bytes; maximum is {max}")]
    EncodedTooLarge {
        /// Value name.
        kind: &'static str,
        /// Actual byte length.
        actual: usize,
        /// Maximum byte length.
        max: usize,
    },
    /// A persisted outbox id was forged or corrupted.
    #[error("outbox id is not derived from its occurrence")]
    InvalidOutboxId { id: OutboxId },
    /// A persisted plan or state could not be encoded.
    #[error("protocol serialization failed: {0}")]
    Serialization(String),
    /// A persisted input, state, or nested value could not be decoded.
    #[error("protocol deserialization failed: {0}")]
    Deserialization(String),
}
