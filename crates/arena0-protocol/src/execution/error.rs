use crate::exec::ExecLifecycle;
use crate::{ActivationError, PeerId, StateHash};

/// Errors raised while validating or advancing one execution aggregate.
///
/// These errors describe protocol invariants, not a classification of event
/// or effect visibility. Every event is dispatched through the same program
/// path; the errors below only reject malformed evidence or an ambiguous
/// durable boundary.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    /// An event is not legal for the current lifecycle.
    #[error("event {event} is illegal while execution is {current:?}")]
    IllegalLifecycle {
        /// Current lifecycle projection.
        current: ExecLifecycle,
        /// Stable event name.
        event: &'static str,
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
    /// Version overflow prevents another durable action.
    #[error("execution version exhausted")]
    VersionExhausted,
    /// Agreed-step coordinate overflow prevents another agreed step.
    #[error("agreed step exhausted")]
    AgreedStepExhausted,
    /// An agreed step did not use the next coordinate.
    #[error("step coordinate mismatch: expected {expected}, got {actual}")]
    StepCoordinateMismatch { expected: u64, actual: u64 },
    /// An agreed step started from a different shared-state hash.
    #[error("agreed pre-state mismatch")]
    AgreedPreStateMismatch {
        /// Expected pre-state hash.
        expected: StateHash,
        /// Supplied pre-state hash.
        actual: StateHash,
    },
    /// An agreed step linked to a different preceding commitment.
    #[error("agreed commitment chain link mismatch")]
    AgreedChainMismatch,
    /// The committed shared bytes do not match their state hash.
    #[error("shared state hash mismatch")]
    StateHashMismatch,
    /// A shared proposal already exists.
    #[error("a shared proposal is already pending")]
    SharedProposalExists,
    /// A local signature makes a pending shared proposal irrevocable.
    #[error("a shared proposal with a local signature cannot be stopped")]
    SharedProposalSigned,
    /// A shared signature arrived without a proposal.
    #[error("no shared proposal is pending")]
    SharedProposalMissing,
    /// The current trace format is unsupported.
    #[error("unsupported trace version {actual}, expected {expected}")]
    TraceVersionMismatch { actual: u32, expected: u32 },
    /// Step zero must carry SessionStarted.
    #[error("step zero is missing SessionStarted")]
    MissingSessionStart,
    /// SessionStarted appeared after step zero.
    #[error("SessionStarted is only legal at step zero")]
    SessionStartPosition,
    /// The start event's ensemble differs from the activation participants.
    #[error("SessionStarted ensemble does not match the activation")]
    SessionStartMismatch,
    /// A trace entry exceeded its encoded bound.
    #[error("trace entry is {actual} bytes; maximum is {max}")]
    TraceEntryTooLarge { actual: usize, max: usize },
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
    /// A signature did not match a pending step commitment.
    #[error("signature for {participant} does not match step {step}")]
    InvalidStepSignature { participant: PeerId, step: u64 },
    /// A duplicate step signature repeated the same content.
    #[error("duplicate step signature from {participant}")]
    DuplicateStepSignature { participant: PeerId },
    /// A duplicate step signature conflicted with prior content.
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
    /// A complete receipt body does not match the execution binding.
    #[error("receipt body does not match the execution binding or terminal certificate")]
    ReceiptBodyMismatch,
    /// A guest signing request is malformed or lacks its kernel binding.
    #[error("invalid guest signing request")]
    InvalidGuestSignData,
    /// A signed abort occurrence is malformed.
    #[error("invalid execution abort occurrence")]
    InvalidAbortOccurrence,
    /// A signed abort occurrence's identity signature is malformed.
    #[error("invalid execution abort signature: {0}")]
    InvalidAbortSignature(String),
    /// An abort occurrence does not identify the exact current agreed head.
    #[error("invalid execution abort coordinate")]
    InvalidAbortCoordinate,
    /// An abort occurrence is not signed by an activation participant.
    #[error("execution abort occurrence is not activation-authenticated")]
    UnauthenticatedAbort,
    /// A complete receipt trace does not contain the expected terminal edge.
    #[error("receipt trace does not match terminal proof")]
    TerminalTraceMismatch,
    /// The guest's agent-facing JSON outcome was malformed.
    #[error("invalid terminal outcome JSON projection: {0}")]
    InvalidOutcomeProjection(String),
    /// A successful terminal entry did not carry an outcome projection.
    #[error("successful terminal entry requires a complete outcome projection")]
    TerminalOutcomeRequired,
    /// A terminal outcome projection does not match the entry.
    #[error("terminal outcome projection does not match the shared entry")]
    TerminalOutcomeMismatch,
    /// The Borsh outcome differs from SessionEnd's bytes.
    #[error("terminal outcome Borsh bytes do not match the SessionEnd effect")]
    OutcomeProjectionMismatch,
    /// A local answer or signature did not identify the stored continuation.
    #[error("continuation does not match the pending continuation")]
    PendingContinuationMismatch,
    /// A continuation effect is malformed.
    #[error("continuation effect is invalid")]
    InvalidPendingContinuation,
    /// Terminal proof is already complete.
    #[error("terminal proof is already published")]
    TerminalAlreadyPublished,
    /// Durable terminal status is missing or inconsistent.
    #[error("durable terminal status is invalid or inconsistent")]
    InvalidTerminalStatus,
    /// A step contained more than one lifecycle effect.
    #[error("trace entry contains multiple terminal effects")]
    MultipleTerminalEffects,
    /// A non-terminal step appeared after terminal proof preparation.
    #[error("terminal proof is pending and cannot accept another step")]
    TerminalProofPending,
    /// The execution already has terminal lifecycle.
    #[error("execution is terminal")]
    AlreadyTerminal,
    /// One dispatch attempted to emit more than one broadcast.
    #[error("one event dispatch contains multiple broadcasts")]
    MultipleBroadcasts,
    /// A timer firing did not match an active timer identity.
    #[error("timer firing does not match an active timer")]
    StaleTimerFiring,
    /// A timer deadline overflowed the supplied deterministic base time.
    #[error("timer deadline overflowed")]
    TimerDeadlineOverflow,
    /// An active timer set is not canonical.
    #[error("active timer set is not canonical")]
    TimerSetNotCanonical,
    /// The active timer count exceeded its bound.
    #[error("active timer set has {actual} entries; maximum is {max}")]
    TooManyTimers { actual: usize, max: usize },
    /// A bounded opaque payload exceeded its limit.
    #[error("{kind} is {actual} bytes; maximum is {max}")]
    PayloadTooLarge {
        /// Payload name.
        kind: &'static str,
        /// Actual byte length.
        actual: usize,
        /// Maximum byte length.
        max: usize,
    },
    /// An encoded value exceeded its total limit.
    #[error("{kind} is {actual} bytes; maximum is {max}")]
    EncodedTooLarge {
        /// Value name.
        kind: &'static str,
        /// Actual byte length.
        actual: usize,
        /// Maximum byte length.
        max: usize,
    },
    /// A persisted protocol value could not be encoded.
    #[error("protocol serialization failed: {0}")]
    Serialization(String),
    /// A persisted protocol value could not be decoded.
    #[error("protocol deserialization failed: {0}")]
    Deserialization(String),
}
