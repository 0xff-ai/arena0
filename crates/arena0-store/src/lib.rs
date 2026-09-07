//! Authoritative per-Host SQLite persistence.
//!
//! [`Store`] owns one SQLite connection on one named blocking owner thread.
//! [`StoreHandle`] is a cloneable, typed asynchronous capability for bounded
//! registry and receipt-artifact operations plus read projections. The queue
//! has both an item limit and a byte budget; a command cannot enter the owner
//! queue until it has reserved its encoded byte cost.
//!
//! The protocol reducer remains the sole owner of execution-state and commit
//! plan construction.  This crate only validates, checks lifetime evidence,
//! performs the SQLite transaction, and materializes the projections that are
//! needed for recovery and scheduling.

use arena0_protocol::PendingId;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::sync::{Arc, Mutex as StdMutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arena0_crypto::{BlsSignature, ExecutionSalt};
use arena0_program::{JsonBytes, ProgramHash};
use arena0_protocol::execution::{
    CommitPlan, DurableEffect, ExecutionInput, ExecutionState, ExecutionVersion, GuestSignData,
    OccurrenceConflict, OccurrenceDigest, OccurrenceEvidence, OccurrenceKey, OutboxId,
    ParticipantStepSignature, ParticipantTerminalSignature, ReceiptArtifact, ReceiptId,
    SharedDelta, TimerId, TimerMutation, TransitionOutcome,
};
use arena0_protocol::{
    Activation, ExecFrame, ExecId, ExecLifecycle, ExecutionAdmission, LocalStateBytes, MessageId,
    NegotiationTarget, PeerId, PreparedActivation, PrivateEffect, PrivateEvent, ProtocolError,
    SessionHash, SharedStateBytes, StateHash, StepCommitment, TerminalCommitment,
    WitnessCommitment,
};
use borsh::{BorshDeserialize, BorshSerialize};
use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;
use tokio::sync::{Semaphore, mpsc, oneshot};

mod codec;
mod database;
mod lock;
use codec::*;
use database::owner_loop;
use lock::{
    OwnerLock, acquire_process_lock, configure_connection, initialize_schema, prepare_database_file,
};

const SCHEMA_VERSION: u64 = 2;
const ENVELOPE_VERSION: u16 = 1;
const ENVELOPE_MAGIC: [u8; 8] = *b"AR0STOR1";
const ENVELOPE_DOMAIN: &[u8] = b"arena0/store-envelope/v1";
const DEFAULT_QUEUE_CAPACITY: usize = 64;
// Command costs include the encoded payload plus a bounded response/metadata
// allowance. Keep the default at least as large as the largest legal program
// registration plus that allowance so a valid maximal artifact is admissible.
const MAX_COMMAND_OVERHEAD: usize = 1_024;
const DEFAULT_QUEUE_BYTES: usize = arena0_program::PROGRAM_MAX_LEN as usize + MAX_COMMAND_OVERHEAD;
const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_LEASE_DURATION_MS: u64 = 30_000;
const DEFAULT_RETRY_DELAY_MS: u64 = 1_000;
const MAX_ERROR_BYTES: usize = 4 * 1024;
const MAX_ADMISSION_BYTES: usize = 4 * 1024;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ENVELOPE_OVERHEAD: usize = 8 + 2 + 2 + 4 + 32;
const EXECUTION_WORKING_SET_BYTES: usize = 64 * 1024 * 1024;
const MAX_USER_AGENT_BYTES: usize = 256;
const USER_AGENT_COMMAND_OVERHEAD: usize = 512;

/// Maximum number of private commit summaries returned by one inspection page.
/// The durable store may contain more records; callers use the returned cursor
/// to request another bounded page.
pub const MAX_PRIVATE_INSPECTION_RECORDS: usize = 256;

/// Configuration for one Host-owned SQLite database.
#[derive(Debug, Clone)]
pub struct StoreConfig {
    path: PathBuf,
    host_id: PeerId,
    queue_capacity: usize,
    queue_bytes: usize,
    busy_timeout: Duration,
    lease_duration_ms: u64,
    retry_delay_ms: u64,
}

impl StoreConfig {
    /// Bind a store to a database path and the owning Host identity.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, host_id: PeerId) -> Self {
        Self {
            path: path.into(),
            host_id,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            queue_bytes: DEFAULT_QUEUE_BYTES,
            busy_timeout: DEFAULT_BUSY_TIMEOUT,
            lease_duration_ms: DEFAULT_LEASE_DURATION_MS,
            retry_delay_ms: DEFAULT_RETRY_DELAY_MS,
        }
    }

    /// Set the maximum number of queued commands.
    #[must_use]
    pub const fn with_queue_capacity(mut self, capacity: usize) -> Self {
        self.queue_capacity = capacity;
        self
    }

    /// Set the maximum encoded bytes retained by the command queue.
    #[must_use]
    pub const fn with_queue_bytes(mut self, bytes: usize) -> Self {
        self.queue_bytes = bytes;
        self
    }

    /// Set SQLite's busy timeout.
    #[must_use]
    pub const fn with_busy_timeout(mut self, timeout: Duration) -> Self {
        self.busy_timeout = timeout;
        self
    }

    /// Set the lease duration used by outbox reservations.
    pub fn with_lease_duration(self, duration: Duration) -> Result<Self, StoreError> {
        let millis = u64::try_from(duration.as_millis()).map_err(|_| {
            StoreError::InvalidConfiguration("lease duration milliseconds must fit u64")
        })?;
        Ok(self.with_lease_duration_ms(millis))
    }

    /// Set the lease duration in milliseconds.
    #[must_use]
    pub const fn with_lease_duration_ms(mut self, duration_ms: u64) -> Self {
        self.lease_duration_ms = duration_ms;
        self
    }

    /// Set the fixed delay before a retried outbox item is ready.
    #[must_use]
    pub const fn with_retry_delay_ms(mut self, delay_ms: u64) -> Self {
        self.retry_delay_ms = delay_ms;
        self
    }
}

/// Summary of lease recovery performed while opening or reconciling a store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RecoveryReport {
    /// Number of expired outbox leases returned to `pending`.
    pub expired_outbox_leases: u64,
}

/// Result of inserting one content-addressed Wasm program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramStoreOutcome {
    /// The content was newly persisted.
    Stored,
    /// The exact content was already present.
    AlreadyStored,
    /// The exact content was present but had been unregistered and is active again.
    Reactivated,
    /// A content hash was already bound to different bytes.
    Conflict,
}

/// Result of unregistering one content-addressed program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramRemoveOutcome {
    /// New-admission membership was removed; artifact bytes remain durable.
    Removed,
    /// It was already absent from new-admission membership.
    AlreadyRemoved,
}

/// One bounded program registry entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredProgram {
    hash: ProgramHash,
    wasm: Vec<u8>,
}

/// Immutable local admission request retained from the first Host call until
/// execution recovery. The optional failure is a terminal diagnostic owned by
/// this request; it never changes the request's program or parameter bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionRequest {
    execution_id: ExecId,
    program_hash: ProgramHash,
    params: Option<JsonBytes>,
    admission: ExecutionAdmission,
    created_order: u64,
    created_at_ms: u64,
    failure: Option<String>,
}

/// Position of one row in the durable execution-request order.
///
/// The cursor is opaque so callers cannot confuse a request's creation order
/// with an execution identity or a protocol version. `Default` starts before
/// the first request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct RecoveryCursor(u64);

impl RecoveryCursor {
    /// Return the cursor before the first durable request.
    #[must_use]
    pub const fn start() -> Self {
        Self(0)
    }

    /// Return the durable request-order position.
    #[must_use]
    pub const fn position(self) -> u64 {
        self.0
    }

    pub(crate) const fn from_position(position: u64) -> Self {
        Self(position)
    }
}

/// One execution with unfinished protocol, proof, or outbox work and the
/// bounded row metadata needed to resume it.
///
/// The store builds this projection from the request, activation, execution,
/// and program relations in one ordered read. `program` is a presence marker,
/// not the Wasm payload. Activation and execution payloads are deliberately
/// absent: the daemon loads each bounded aggregate separately and validates it
/// before registering a live execution handle. Keeping those payloads out of a
/// page makes the page response bound independent of the maximum execution
/// state size.
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryCandidate {
    cursor: RecoveryCursor,
    request: ExecutionRequest,
    activation_status: Option<ActivationRecordStatus>,
    session_id: Option<SessionHash>,
    execution_present: bool,
    program: Option<ProgramHash>,
}

impl RecoveryCandidate {
    /// Return the cursor position represented by this candidate.
    #[must_use]
    pub const fn cursor(&self) -> RecoveryCursor {
        self.cursor
    }

    /// Borrow the immutable admission request.
    #[must_use]
    pub const fn request(&self) -> &ExecutionRequest {
        &self.request
    }

    /// Return the durable activation status, if preparation has started.
    #[must_use]
    pub const fn activation_status(&self) -> Option<ActivationRecordStatus> {
        self.activation_status
    }

    /// Return the activated session identity, if preparation has started.
    #[must_use]
    pub const fn session_id(&self) -> Option<SessionHash> {
        self.session_id
    }

    /// Whether an execution aggregate exists for this request.
    #[must_use]
    pub const fn has_execution(&self) -> bool {
        self.execution_present
    }

    /// Return the program hash when the corresponding registry row exists.
    ///
    /// The method reports row presence only. Callers must still load and parse
    /// the immutable Wasm before constructing an execution actor.
    #[must_use]
    pub const fn program(&self) -> Option<ProgramHash> {
        self.program
    }
}

/// One bounded page of ordered, resumable execution candidates.
#[derive(Debug, Clone, PartialEq)]
pub struct RecoveryPage {
    candidates: Vec<RecoveryCandidate>,
    next: Option<RecoveryCursor>,
}

impl RecoveryPage {
    /// Borrow the candidates in ascending durable request order.
    #[must_use]
    pub fn candidates(&self) -> &[RecoveryCandidate] {
        &self.candidates
    }

    /// Consume the page and return its candidates.
    #[must_use]
    pub fn into_candidates(self) -> Vec<RecoveryCandidate> {
        self.candidates
    }

    /// Return the cursor for the next page, if this page filled its limit.
    #[must_use]
    pub const fn next_cursor(&self) -> Option<RecoveryCursor> {
        self.next
    }

    /// Whether the page contains no resumable candidates.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.candidates.is_empty()
    }

    pub(crate) const fn new(
        candidates: Vec<RecoveryCandidate>,
        next: Option<RecoveryCursor>,
    ) -> Self {
        Self { candidates, next }
    }
}

impl ExecutionRequest {
    /// Return the local execution identity.
    #[must_use]
    pub const fn execution_id(&self) -> ExecId {
        self.execution_id
    }

    /// Return the admitted program content address.
    #[must_use]
    pub const fn program_hash(&self) -> ProgramHash {
        self.program_hash
    }

    /// Borrow the preferred agent-facing parameter bytes, if supplied.
    #[must_use]
    pub fn params(&self) -> Option<&JsonBytes> {
        self.params.as_ref()
    }

    /// Borrow the exact local admission authority.
    #[must_use]
    pub const fn admission(&self) -> &ExecutionAdmission {
        &self.admission
    }

    /// Return the negotiation identity derived from the admission authority.
    #[must_use]
    pub const fn negotiation_id(&self) -> Option<arena0_protocol::NegotiationId> {
        self.admission.negotiation_id()
    }

    /// Return the monotonic local creation order.
    #[must_use]
    pub const fn created_order(&self) -> u64 {
        self.created_order
    }

    /// Return the request creation time supplied by the Host.
    #[must_use]
    pub const fn created_at_ms(&self) -> u64 {
        self.created_at_ms
    }

    /// Borrow a terminal request failure, if recorded.
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }
}

/// Result of inserting or re-reading an immutable local admission request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionRequestOutcome {
    /// A new request root was created.
    Created,
    /// The exact request root already existed.
    AlreadyExists,
    /// The execution identity was already bound to different request facts.
    Conflict,
}

/// Result of binding an open Join request to its first accepted offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionBindingOutcome {
    /// The open request now names this exact creator and negotiation.
    Bound,
    /// The same target was already durably selected.
    AlreadyBound,
    /// A different target was already selected.
    Conflict,
}

/// Result of recording a terminal request failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionRequestFailureOutcome {
    /// The failure was newly recorded.
    Recorded,
    /// The same failure was already recorded.
    AlreadyRecorded,
    /// A different failure is already bound to the request.
    Conflict,
}

impl StoredProgram {
    /// Return the content address.
    #[must_use]
    pub const fn hash(&self) -> ProgramHash {
        self.hash
    }

    /// Borrow the exact Wasm bytes.
    #[must_use]
    pub fn wasm(&self) -> &[u8] {
        &self.wasm
    }
}

/// Errors returned by the concrete SQLite store.
#[derive(Debug, Error)]
pub enum StoreError {
    /// SQLite rejected an operation.
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Filesystem or lock-file operation failed.
    #[error("store I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Another process currently owns this database.
    #[error("store database is already owned: {path}")]
    AlreadyOwned { path: PathBuf },
    /// The owner thread or its connection has closed.
    #[error("store owner is closed")]
    Closed,
    /// A typed command reply was dropped.
    #[error("store owner dropped the command reply")]
    ReplyDropped,
    /// The owner thread could not be joined.
    #[error("store owner thread panicked")]
    OwnerPanicked,
    /// The database does not use the exact schema understood by this crate.
    #[error("unsupported store schema version {0}")]
    UnsupportedSchema(u64),
    /// The database path, SQLite configuration, or concrete schema is invalid.
    #[error("invalid store database: {0}")]
    InvalidDatabase(String),
    /// The database is bound to a different Host identity.
    #[error("store identity mismatch: database belongs to {database}, requested {requested}")]
    IdentityMismatch { database: PeerId, requested: PeerId },
    /// A live execution writer already exists for this execution identity.
    #[error("execution {0} already has a live store writer")]
    ExecutionAlreadyClaimed(ExecId),
    /// The requested execution does not exist.
    #[error("execution {0} was not found")]
    ExecutionNotFound(ExecId),
    /// The requested activation record does not exist.
    #[error("activation for execution {0} was not found")]
    ActivationNotFound(ExecId),
    /// The local admission root does not exist.
    #[error("execution request {0} was not found")]
    ExecutionRequestNotFound(ExecId),
    /// Request failure is legal only before activation creates the durable
    /// execution lifecycle authority.
    #[error("execution {0} has already entered activation")]
    ExecutionLifecycleStarted(ExecId),
    /// Execution creation requires a committed activation record.
    #[error("activation for execution {0} is not committed")]
    ActivationNotCommitted(ExecId),
    /// A receipt was not found.
    #[error("receipt {0:?} was not found")]
    ReceiptNotFound(ReceiptId),
    /// The requested outbox occurrence does not exist.
    #[error("outbox occurrence {0:?} was not found")]
    OutboxNotFound(OutboxId),
    /// The database contains malformed, tampered, or internally inconsistent data.
    #[error("store corruption: {0}")]
    Corruption(String),
    /// A protocol value failed its construction or validation invariant.
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    /// A command exceeds the configured byte budget.
    #[error("store command requires {required} bytes, queue budget is {capacity}")]
    CommandTooLarge { required: usize, capacity: usize },
    /// A store configuration is invalid.
    #[error("invalid store configuration: {0}")]
    InvalidConfiguration(&'static str),
    /// A local admission request does not authorize the proposed activation.
    #[error("invalid execution admission: {0}")]
    InvalidAdmission(String),
    /// A frame could not be authenticated against its transport source.
    #[error("inbound frame source is not authenticated: {0}")]
    UnauthenticatedSource(String),
    /// A frame was not durably accepted before an input tried to apply it.
    #[error("inbound frame {0:?} was not durably accepted")]
    InboxNotAccepted(InboxId),
    /// An input does not correspond to the selected frame part.
    #[error("input does not correspond to inbound frame {inbox_id:?} part {part_index}")]
    InboxInputMismatch { inbox_id: InboxId, part_index: u32 },
    /// The selected inbound message requires an explicit guest shared delta.
    #[error("inbound message {0:?} requires explicit message resolution")]
    InboxMessageNeedsResolution(InboxId),
    /// The selected inbound frame is not a message and cannot use message resolution.
    #[error("inbound frame {0:?} is not a message")]
    InboxNotMessage(InboxId),
    /// Receipt bodies must be derived from the store's authoritative rows.
    #[error("receipt bodies must be assembled by the store")]
    ReceiptBodyRequiresAssembly,
}

/// Derived provenance of a receipt artifact retained by this Host.
///
/// The store records import and local-production facts independently. This
/// value is their total projection, so an artifact can retain both facts
/// instead of one operation overwriting the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptProvenance {
    /// The local Host produced this artifact.
    Produced,
    /// The artifact was imported from another Host.
    Imported,
    /// The artifact was imported and was also produced locally.
    Both,
}

impl ReceiptProvenance {
    fn from_facts(imported: bool, produced: bool) -> Result<Self, StoreError> {
        match (imported, produced) {
            (false, true) => Ok(Self::Produced),
            (true, false) => Ok(Self::Imported),
            (true, true) => Ok(Self::Both),
            (false, false) => Err(StoreError::Corruption(
                "receipt artifact has no provenance fact".into(),
            )),
        }
    }

    /// Whether this artifact came from another Host.
    #[must_use]
    pub const fn is_imported(self) -> bool {
        matches!(self, Self::Imported | Self::Both)
    }

    /// Whether this Host produced this artifact.
    #[must_use]
    pub const fn is_produced(self) -> bool {
        matches!(self, Self::Produced | Self::Both)
    }
}

/// Result of importing one portable receipt artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReceiptImportOutcome {
    /// The foreign artifact was newly persisted.
    Imported,
    /// The exact foreign artifact was already persisted.
    AlreadyImported,
    /// The exact artifact was already present as a locally produced receipt.
    AlreadyProduced,
}

/// Result of inserting or re-reading one execution aggregate.
#[derive(Debug, Clone, PartialEq)]
pub enum CreateExecutionOutcome {
    /// A new aggregate was durably inserted.
    Created(ExecutionState),
    /// The same aggregate already existed byte-for-byte.
    AlreadyExists(ExecutionState),
}

/// Lifecycle state of a permanent activation record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationRecordStatus {
    /// Candidate activation is durable and resumable, but not executable.
    Prepared,
    /// Candidate activation is complete and may create execution state.
    Committed,
}

/// Permanent activation evidence keyed by one local execution identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationRecord {
    execution_id: ExecId,
    state: ActivationRecordState,
}

/// The event kind in one host-local private handler commit. Payloads remain
/// private; this enum is the store's safe diagnostic projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivateEventKind {
    InputReceived,
    TimerFired,
    TypedTimerFired,
    Signed,
    React,
}

/// One private effect's kind and bounded payload size. The store never returns
/// the effect payload itself from inspection reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrivateEffectSummary {
    pub kind: PrivateEffectKind,
    pub payload_bytes: Option<usize>,
}

/// The effect kind in one host-local private handler commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrivateEffectKind {
    Broadcast,
    Callout,
    SetTimer,
    Sign,
    RetryInput,
}

/// A safe projection of one durable private commit for local diagnostics.
/// Sequence and public position are authoritative durable coordinates; input
/// and effects contain only kinds and payload sizes, never raw private values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateCommitSummary {
    pub sequence: u64,
    pub public_position: u64,
    pub event: PrivateEventKind,
    pub input_payload_bytes: Option<usize>,
    pub effects: Vec<PrivateEffectSummary>,
    pub fuel_used: u64,
}

/// One bounded page of private commit summaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateInspectionPage {
    from: u64,
    summaries: Vec<PrivateCommitSummary>,
    total: u64,
    next: Option<u64>,
}

impl PrivateInspectionPage {
    /// Return the first private sequence represented by this page.
    #[must_use]
    pub const fn from(&self) -> u64 {
        self.from
    }

    /// Borrow summaries in ascending private sequence order.
    #[must_use]
    pub fn summaries(&self) -> &[PrivateCommitSummary] {
        &self.summaries
    }

    /// Consume the page and return its summaries.
    #[must_use]
    pub fn into_summaries(self) -> Vec<PrivateCommitSummary> {
        self.summaries
    }

    /// Return the durable number of private commits at read time.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// Return the next private sequence when another page remains.
    #[must_use]
    pub const fn next(&self) -> Option<u64> {
        self.next
    }

    pub(crate) const fn new(
        from: u64,
        summaries: Vec<PrivateCommitSummary>,
        total: u64,
        next: Option<u64>,
    ) -> Self {
        Self {
            from,
            summaries,
            total,
            next,
        }
    }
}

impl PrivateCommitSummary {
    fn from_commit(commit: &arena0_protocol::PrivateCommit) -> Self {
        let record = commit.record();
        let (event, input_payload_bytes) = match &record.event {
            PrivateEvent::InputReceived { data, .. } => {
                (PrivateEventKind::InputReceived, Some(data.len()))
            }
            PrivateEvent::TimerFired => (PrivateEventKind::TimerFired, None),
            PrivateEvent::TypedTimerFired { timer } => {
                (PrivateEventKind::TypedTimerFired, Some(timer.data.len()))
            }
            PrivateEvent::Signed { signature, .. } => {
                (PrivateEventKind::Signed, Some(signature.len()))
            }
            PrivateEvent::React => (PrivateEventKind::React, None),
        };
        let effects = record
            .effects
            .iter()
            .map(|effect| match effect {
                PrivateEffect::Broadcast { data } => PrivateEffectSummary {
                    kind: PrivateEffectKind::Broadcast,
                    payload_bytes: Some(data.len()),
                },
                PrivateEffect::Callout { context, .. } => PrivateEffectSummary {
                    kind: PrivateEffectKind::Callout,
                    payload_bytes: Some(context.len()),
                },
                PrivateEffect::SetTimer { timer, .. } => PrivateEffectSummary {
                    kind: PrivateEffectKind::SetTimer,
                    payload_bytes: timer.as_ref().map(|timer| timer.data.len()),
                },
                PrivateEffect::Sign { data, .. } => PrivateEffectSummary {
                    kind: PrivateEffectKind::Sign,
                    payload_bytes: Some(data.len()),
                },
                PrivateEffect::RetryInput { reason } => PrivateEffectSummary {
                    kind: PrivateEffectKind::RetryInput,
                    payload_bytes: Some(reason.len()),
                },
            })
            .collect();
        Self {
            sequence: record.seq,
            public_position: record.after_position,
            event,
            input_payload_bytes,
            effects,
            fuel_used: record.fuel_used,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ActivationRecordState {
    Prepared {
        evidence: PreparedActivation,
    },
    Committed {
        evidence: PreparedActivation,
        activation: Box<Activation>,
    },
}

impl ActivationRecord {
    /// Return the local execution identity.
    #[must_use]
    pub const fn execution_id(&self) -> ExecId {
        self.execution_id
    }

    /// Borrow the exact unsigned activation evidence fixed during preparation.
    #[must_use]
    pub const fn prepared(&self) -> &PreparedActivation {
        match &self.state {
            ActivationRecordState::Prepared { evidence }
            | ActivationRecordState::Committed { evidence, .. } => evidence,
        }
    }

    /// Borrow the final certified activation, when commitment completed.
    #[must_use]
    pub const fn activation(&self) -> Option<&Activation> {
        match &self.state {
            ActivationRecordState::Prepared { .. } => None,
            ActivationRecordState::Committed { activation, .. } => Some(activation),
        }
    }

    /// Return the activation's stable session identity.
    #[must_use]
    pub fn session_id(&self) -> SessionHash {
        self.prepared().session_hash()
    }

    /// Return the durable activation status.
    #[must_use]
    pub const fn status(&self) -> ActivationRecordStatus {
        match &self.state {
            ActivationRecordState::Prepared { .. } => ActivationRecordStatus::Prepared,
            ActivationRecordState::Committed { .. } => ActivationRecordStatus::Committed,
        }
    }

    /// Whether the activation may create an execution aggregate.
    #[must_use]
    pub const fn is_committed(&self) -> bool {
        matches!(self.status(), ActivationRecordStatus::Committed)
    }
}

/// Outcome of preparing a permanent activation record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrepareActivationOutcome {
    /// A candidate was inserted.
    Prepared(Box<ActivationRecord>),
    /// An identical candidate was already prepared.
    AlreadyPrepared(Box<ActivationRecord>),
    /// The exact candidate was already committed.
    AlreadyCommitted(Box<ActivationRecord>),
    /// The execution identity was already bound to different evidence.
    Conflict {
        existing: Box<ActivationRecord>,
        incoming: Box<ActivationRecord>,
    },
}

/// Outcome of committing a prepared activation record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitActivationOutcome {
    /// The candidate was atomically marked committed.
    Committed(Box<ActivationRecord>),
    /// It was already committed with identical evidence.
    AlreadyCommitted(Box<ActivationRecord>),
    /// The execution identity was bound to different evidence.
    Conflict {
        existing: Box<ActivationRecord>,
        incoming: Box<ActivationRecord>,
    },
}

/// Outcome of applying one validated execution input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommittedSummary {
    version: ExecutionVersion,
    /// The public trace step certified by this commit, if the public cursor
    /// advanced. A shared proposal and partial signature commit leave this
    /// unset; only the complete N-of-N signature commit sets it.
    public_step: Option<u64>,
}

impl CommittedSummary {
    #[must_use]
    pub const fn version(self) -> ExecutionVersion {
        self.version
    }

    /// Return the public trace step that became durable, if any.
    #[must_use]
    pub const fn public_step(self) -> Option<u64> {
        self.public_step
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ApplyOutcome {
    /// The reducer-derived plan and all durable consequences committed.
    Committed(CommittedSummary),
    /// The exact occurrence was already durably applied.
    AlreadyApplied,
    /// The semantic occurrence slot was reused with a different input digest.
    Conflict(OccurrenceConflict),
    /// A compare-and-set found another committed version.
    VersionMismatch {
        expected: ExecutionVersion,
        actual: ExecutionVersion,
    },
    /// The frame source was accepted but has already been applied.
    InboxAlreadyApplied {
        inbox_id: InboxId,
        version: ExecutionVersion,
    },
    /// The frame was consumed without a reducer input.
    InboxAlreadyConsumed { inbox_id: InboxId },
}

/// A typed result for accepting one authenticated inbound frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxAcceptOutcome {
    /// The frame and source attribution were durably accepted.
    Accepted,
    /// The exact frame was already accepted or applied.
    AlreadyAccepted,
    /// The exact frame was already applied by the execution transaction.
    AlreadyApplied,
    /// The exact frame was already consumed without a reducer input.
    AlreadyConsumed,
    /// The frame id was reused with different durable evidence.
    Conflict,
}

/// Result of explicitly rejecting any accepted inbound frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxRejectOutcome {
    /// The frame was newly marked consumed.
    Rejected,
    /// The frame was already marked consumed.
    AlreadyRejected,
    /// The frame was already applied by a reducer transaction.
    AlreadyApplied,
}

/// A source-labelled frame whose source is supplied by the authenticated
/// transport.  Store validation additionally checks claimed message/abort
/// identities before it records the frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthenticatedFrame {
    source: PeerId,
    frame: ExecFrame,
}

/// Store-domain identity of one authenticated inbound frame.
///
/// This is deliberately distinct from protocol [`arena0_protocol::execution::FrameId`] values used by
/// durable effects.  The authenticated source is part of the identity, so
/// identical frame bytes arriving over two authenticated routes cannot occupy
/// one another's inbox slot.
#[derive(
    BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
pub struct InboxId([u8; 32]);

impl InboxId {
    /// Construct an inbox identity from persisted bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow the identity bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// One accepted inbound frame awaiting its execution reducer input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInboxItem {
    execution_id: ExecId,
    inbox_id: InboxId,
    source: PeerId,
    frame: ExecFrame,
}

impl PendingInboxItem {
    /// Return the owning execution.
    #[must_use]
    pub const fn execution_id(&self) -> ExecId {
        self.execution_id
    }

    /// Return the authenticated inbox identity.
    #[must_use]
    pub const fn inbox_id(&self) -> InboxId {
        self.inbox_id
    }

    /// Return the authenticated source peer.
    #[must_use]
    pub const fn source(&self) -> PeerId {
        self.source
    }

    /// Borrow the canonical stored execution frame.
    #[must_use]
    pub const fn frame(&self) -> &ExecFrame {
        &self.frame
    }
}

/// Stable lease identity returned with one leased outbox item.
#[derive(
    BorshSerialize, BorshDeserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
pub struct LeaseId([u8; 32]);

impl LeaseId {
    /// Construct a lease identity from persisted bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrow the identity bytes.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Current outbox delivery state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxStatus {
    /// Available for delivery.
    Pending,
    /// Reserved by one delivery worker.
    Leased,
    /// Delivery was durably acknowledged.
    Acknowledged,
}

/// One decoded outbox occurrence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxItem {
    /// Stable effect occurrence identity.
    pub outbox_id: OutboxId,
    /// Owning execution.
    pub execution_id: ExecId,
    /// Version that emitted the effect.
    pub version: ExecutionVersion,
    /// Position within that plan's effect list.
    pub ordinal: u32,
    /// Typed durable effect.
    pub effect: DurableEffect,
    /// Number of lease attempts so far; informational, never a dead-letter policy.
    pub attempts: u32,
    /// Current state.
    pub status: OutboxStatus,
    /// Earliest time at which delivery may begin.
    pub available_at_ms: u64,
}

/// One outbox occurrence reserved by a lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeasedOutbox {
    /// Leased occurrence data.
    pub item: OutboxItem,
    /// Lease identity required for completion or retry.
    pub lease_id: LeaseId,
    /// Lease expiry.
    pub lease_until_ms: u64,
}

/// Typed result of an outbox delivery operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboxDeliveryOutcome {
    /// A lease was acknowledged.
    Acknowledged,
    /// The occurrence was already acknowledged.
    AlreadyAcknowledged,
    /// The lease did not own the occurrence.
    LeaseMismatch,
    /// The occurrence was not leased.
    NotLeased,
    /// A retry was durably scheduled.
    Retried { available_at_ms: u64, attempts: u32 },
}

/// One active timer scheduling projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveTimer {
    /// Timer identity.
    pub timer_id: TimerId,
    /// Absolute due time.
    pub deadline_ms: u64,
    /// Opaque timer payload.
    pub payload: Vec<u8>,
    /// Version that armed the timer.
    pub armed_version: ExecutionVersion,
}

/// A stored portable artifact with its content identity and local provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredReceipt {
    /// The content-addressed public receipt identity.
    pub receipt_id: ReceiptId,
    /// Whether this Host imported, produced, or both imported and produced
    /// the artifact.
    pub provenance: ReceiptProvenance,
    /// The validated portable receipt artifact.
    pub receipt: ReceiptArtifact,
}

impl StoredReceipt {
    /// Return the derived durable provenance of this artifact.
    #[must_use]
    pub const fn provenance(&self) -> ReceiptProvenance {
        self.provenance
    }

    /// Return whether this artifact is foreign evidence imported by the Host.
    #[must_use]
    pub const fn is_imported(&self) -> bool {
        self.provenance.is_imported()
    }
}

/// Durable agent-facing work that remains relevant to the current pending
/// continuation after an actor restart.
///
/// Outbox rows are retained after acknowledgement, so the store can recover
/// a request whose message was delivered immediately before a process crash.
/// The projection includes the original callout context or the exact guest
/// signing preimage; no daemon-side copy is needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingRequest {
    /// A typed JSON callout waiting for its agent answer.
    Callout {
        /// Durable outbox identity carrying the original request.
        outbox_id: OutboxId,
        /// Current delivery state of that outbox row.
        status: OutboxStatus,
        /// Pending continuation identity.
        pending_id: PendingId,
        /// Program-local callout variant.
        callout_index: u32,
        /// Exact guest-produced callout context bytes.
        context: Vec<u8>,
        /// Generated expected result type, when available.
        expected_type: Option<String>,
    },
    /// A guest-produced signing request waiting for its signature.
    Signature {
        /// Durable outbox identity carrying the original request.
        outbox_id: OutboxId,
        /// Current delivery state of that outbox row.
        status: OutboxStatus,
        /// Pending continuation identity.
        pending_id: PendingId,
        /// Exact guest-selected signing preimage.
        data: GuestSignData,
    },
}

impl PendingRequest {
    /// Return the durable outbox identity carrying this request.
    #[must_use]
    pub const fn outbox_id(&self) -> OutboxId {
        match self {
            Self::Callout { outbox_id, .. } | Self::Signature { outbox_id, .. } => *outbox_id,
        }
    }

    /// Return the delivery state of the retained outbox row.
    #[must_use]
    pub const fn status(&self) -> OutboxStatus {
        match self {
            Self::Callout { status, .. } | Self::Signature { status, .. } => *status,
        }
    }

    /// Return the pending continuation identity.
    #[must_use]
    pub const fn pending_id(&self) -> PendingId {
        match self {
            Self::Callout { pending_id, .. } | Self::Signature { pending_id, .. } => *pending_id,
        }
    }
}

/// Cloneable asynchronous capability for a single store owner.
///
/// This handle owns bounded registry and receipt-artifact operations plus read
/// projections.
/// Execution mutations require an [`ExecutionStore`] claimed for the target
/// execution, so cloning this handle cannot create a second live writer.
#[derive(Clone, Debug)]
pub struct StoreHandle {
    sender: mpsc::Sender<QueuedCommand>,
    budget: Arc<Semaphore>,
    queue_bytes: usize,
    execution_claims: Arc<StdMutex<HashSet<ExecId>>>,
    host_id: PeerId,
}

/// Non-cloneable writer capability for one execution aggregate.
///
/// The capability is bound to its [`ExecId`] at construction. Every execution
/// mutation on this type uses that bound identity; callers cannot accidentally
/// apply a mutation to a different execution. Dropping it releases the live
/// claim shared by all [`StoreHandle`] clones.
///
/// A shared borrow cannot invoke a state-changing operation:
///
/// ```compile_fail
/// fn shared_writer_cannot_mutate(
///     writer: &arena0_store::ExecutionStore,
///     input: arena0_protocol::ExecutionInput,
/// ) {
///     let _future = writer.apply_input(input, 0);
/// }
/// ```
#[derive(Debug)]
pub struct ExecutionStore {
    handle: StoreHandle,
    execution_id: ExecId,
}

/// The sole owner of one SQLite connection and its blocking thread.
pub struct Store {
    handle: StoreHandle,
    owner: Option<JoinHandle<()>>,
    recovery: RecoveryReport,
}

/// A process reservation for one store database path.
///
/// The reservation owns the store's process lock before the SQLite database
/// is opened. This lets a daemon reserve ownership before it initializes any
/// other per-Host state. A reservation is consumed by [`Self::open`], and
/// dropping it releases the process lock.
#[derive(Debug)]
pub struct StoreReservation {
    path: PathBuf,
    lock: OwnerLock,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Store")
            .field("recovery", &self.recovery)
            .finish_non_exhaustive()
    }
}

enum Command {
    CreateExecutionRequest {
        execution_id: ExecId,
        program_hash: ProgramHash,
        params: Option<JsonBytes>,
        admission: ExecutionAdmission,
        created_at_ms: u64,
        reply: oneshot::Sender<Result<ExecutionRequestOutcome, StoreError>>,
    },
    BindJoinTarget {
        execution_id: ExecId,
        target: NegotiationTarget,
        reply: oneshot::Sender<Result<AdmissionBindingOutcome, StoreError>>,
    },
    LoadExecutionRequest {
        execution_id: ExecId,
        reply: oneshot::Sender<Result<Option<ExecutionRequest>, StoreError>>,
    },
    ListExecutionRequests {
        limit: usize,
        reply: oneshot::Sender<Result<Vec<ExecutionRequest>, StoreError>>,
    },
    ListRecoveryCandidates {
        cursor: RecoveryCursor,
        limit: usize,
        reply: oneshot::Sender<Result<RecoveryPage, StoreError>>,
    },
    RecordExecutionRequestFailure {
        execution_id: ExecId,
        reason: String,
        reply: oneshot::Sender<Result<ExecutionRequestFailureOutcome, StoreError>>,
    },
    LoadOrCreateExecutionSalt {
        execution_id: ExecId,
        now_ms: u64,
        reply: oneshot::Sender<Result<ExecutionSalt, StoreError>>,
    },
    RegisterProgram {
        hash: ProgramHash,
        wasm: Vec<u8>,
        now_ms: u64,
        reply: oneshot::Sender<Result<ProgramStoreOutcome, StoreError>>,
    },
    LoadProgram {
        hash: ProgramHash,
        reply: oneshot::Sender<Result<Option<StoredProgram>, StoreError>>,
    },
    ListPrograms {
        limit: usize,
        reply: oneshot::Sender<Result<Vec<ProgramHash>, StoreError>>,
    },
    RemoveProgram {
        hash: ProgramHash,
        now_ms: u64,
        reply: oneshot::Sender<Result<ProgramRemoveOutcome, StoreError>>,
    },
    PrepareActivation {
        execution_id: ExecId,
        prepared: PreparedActivation,
        now_ms: u64,
        reply: oneshot::Sender<Result<PrepareActivationOutcome, StoreError>>,
    },
    CommitActivation {
        execution_id: ExecId,
        activation: Activation,
        now_ms: u64,
        reply: oneshot::Sender<Result<CommitActivationOutcome, StoreError>>,
    },
    LoadActivation {
        execution_id: ExecId,
        reply: oneshot::Sender<Result<Option<ActivationRecord>, StoreError>>,
    },
    CreateExecution {
        execution_id: ExecId,
        activation: Activation,
        producer: PeerId,
        shared_state: SharedStateBytes,
        local_state: LocalStateBytes,
        now_ms: u64,
        reply: oneshot::Sender<Result<CreateExecutionOutcome, StoreError>>,
    },
    LoadExecution {
        execution_id: ExecId,
        reply: oneshot::Sender<Result<Option<ExecutionState>, StoreError>>,
    },
    LoadExecutionBySession {
        session_id: SessionHash,
        reply: oneshot::Sender<Result<Option<ExecutionState>, StoreError>>,
    },
    ListActivations {
        limit: usize,
        reply: oneshot::Sender<Result<Vec<ActivationRecord>, StoreError>>,
    },
    ListExecutions {
        limit: usize,
        reply: oneshot::Sender<Result<Vec<ExecutionState>, StoreError>>,
    },
    ApplyInput {
        execution_id: ExecId,
        input: Box<ExecutionInput>,
        inbox: Option<InboxReference>,
        now_ms: u64,
        reply: oneshot::Sender<Result<ApplyOutcome, StoreError>>,
    },
    AcceptInbound {
        execution_id: ExecId,
        frame: Box<AuthenticatedFrame>,
        now_ms: u64,
        reply: oneshot::Sender<Result<InboxAcceptOutcome, StoreError>>,
    },
    ListPendingInbox {
        execution_id: ExecId,
        limit: usize,
        reply: oneshot::Sender<Result<Vec<PendingInboxItem>, StoreError>>,
    },
    ListPendingRequests {
        execution_id: ExecId,
        reply: oneshot::Sender<Result<Vec<PendingRequest>, StoreError>>,
    },
    ReadTrace {
        execution_id: ExecId,
        from: u64,
        to: u64,
        reply: oneshot::Sender<Result<Vec<arena0_protocol::TraceEntry>, StoreError>>,
    },
    ReadPrivateSummaries {
        execution_id: ExecId,
        from: Option<u64>,
        limit: usize,
        reply: oneshot::Sender<Result<PrivateInspectionPage, StoreError>>,
    },
    ApplyInbound {
        execution_id: ExecId,
        inbox_id: InboxId,
        now_ms: u64,
        reply: oneshot::Sender<Result<ApplyOutcome, StoreError>>,
    },
    ApplyInboundMessage {
        execution_id: ExecId,
        inbox_id: InboxId,
        delta: Box<SharedDelta>,
        now_ms: u64,
        reply: oneshot::Sender<Result<ApplyOutcome, StoreError>>,
    },
    RejectInbound {
        execution_id: ExecId,
        inbox_id: InboxId,
        now_ms: u64,
        reply: oneshot::Sender<Result<InboxRejectOutcome, StoreError>>,
    },
    LeaseOutbox {
        execution_id: ExecId,
        now_ms: u64,
        reply: oneshot::Sender<Result<Option<LeasedOutbox>, StoreError>>,
    },
    AcknowledgeOutbox {
        execution_id: ExecId,
        outbox_id: OutboxId,
        lease_id: LeaseId,
        reply: oneshot::Sender<Result<OutboxDeliveryOutcome, StoreError>>,
    },
    RetryOutbox {
        execution_id: ExecId,
        outbox_id: OutboxId,
        lease_id: LeaseId,
        now_ms: u64,
        reason: String,
        reply: oneshot::Sender<Result<OutboxDeliveryOutcome, StoreError>>,
    },
    RecoverExpiredLeases {
        execution_id: ExecId,
        now_ms: u64,
        reply: oneshot::Sender<Result<RecoveryReport, StoreError>>,
    },
    DueTimers {
        execution_id: ExecId,
        now_ms: u64,
        limit: usize,
        reply: oneshot::Sender<Result<Vec<ActiveTimer>, StoreError>>,
    },
    AssembleReceipt {
        execution_id: ExecId,
        now_ms: u64,
        reply: oneshot::Sender<Result<ApplyOutcome, StoreError>>,
    },
    ImportReceipt {
        receipt: Box<ReceiptArtifact>,
        now_ms: u64,
        reply: oneshot::Sender<Result<ReceiptImportOutcome, StoreError>>,
    },
    LoadReceipt {
        session_id: SessionHash,
        reply: oneshot::Sender<Result<Option<StoredReceipt>, StoreError>>,
    },
    LoadReceiptById {
        receipt_id: ReceiptId,
        reply: oneshot::Sender<Result<Option<StoredReceipt>, StoreError>>,
    },
    ListReceipts {
        limit: usize,
        reply: oneshot::Sender<Result<Vec<StoredReceipt>, StoreError>>,
    },
    LoadUserAgent {
        reply: oneshot::Sender<Result<Option<String>, StoreError>>,
    },
    SetUserAgent {
        value: String,
        reply: oneshot::Sender<Result<(), StoreError>>,
    },
    Shutdown {
        reply: oneshot::Sender<Result<(), StoreError>>,
    },
}

/// The source and frame identity used to link an input application to an
/// accepted inbox row.  It is kept private to prevent callers from changing
/// source attribution between enqueue and apply.
#[derive(Debug, Clone, Copy)]
struct InboxReference {
    inbox_id: InboxId,
    source: PeerId,
    complete_on_apply: bool,
}

struct QueuedCommand {
    command: Command,
    _budget: tokio::sync::OwnedSemaphorePermit,
}

impl Store {
    /// Reserve process ownership of a store database path.
    ///
    /// The SQLite file is not opened by this operation. The returned
    /// reservation keeps the process lock until it is opened or dropped.
    pub fn reserve(path: impl AsRef<Path>) -> Result<StoreReservation, StoreError> {
        let path = path.as_ref().to_path_buf();
        let lock = acquire_process_lock(&path)?;
        Ok(StoreReservation { path, lock })
    }

    /// Open a database, acquire its process lock, initialize its schema, and start its
    /// one blocking owner thread.
    pub fn open(config: StoreConfig) -> Result<Self, StoreError> {
        validate_config(&config)?;
        Self::reserve(&config.path)?.open(config)
    }

    fn open_reserved(config: StoreConfig, lock: OwnerLock) -> Result<Self, StoreError> {
        let (sender, receiver) = mpsc::channel(config.queue_capacity);
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let owner_config = config.clone();
        let owner = thread::Builder::new()
            .name(format!("arena0-store-{}", short_id(config.host_id)))
            .spawn(move || owner_loop(owner_config, lock, receiver, ready_tx))
            .map_err(StoreError::Io)?;

        let recovery = match ready_rx.recv() {
            Ok(Ok(recovery)) => recovery,
            Ok(Err(error)) => {
                let _ = owner.join();
                return Err(error);
            }
            Err(_) => {
                let _ = owner.join();
                return Err(StoreError::OwnerPanicked);
            }
        };
        let handle = StoreHandle {
            sender,
            budget: Arc::new(Semaphore::new(config.queue_bytes)),
            queue_bytes: config.queue_bytes,
            execution_claims: Arc::new(StdMutex::new(HashSet::new())),
            host_id: config.host_id,
        };
        Ok(Self {
            handle,
            owner: Some(owner),
            recovery,
        })
    }

    /// Borrow the cloneable command handle.
    #[must_use]
    pub const fn handle(&self) -> &StoreHandle {
        &self.handle
    }

    /// Return recovery performed during open.
    #[must_use]
    pub const fn recovery_report(&self) -> RecoveryReport {
        self.recovery
    }

    /// Ask the owner to finish and join its blocking thread.
    pub async fn shutdown(mut self) -> Result<(), StoreError> {
        let (reply, response) = oneshot::channel();
        self.handle.send(Command::Shutdown { reply }, 1).await?;
        response.await.map_err(|_| StoreError::ReplyDropped)??;
        let owner = self.owner.take().ok_or(StoreError::OwnerPanicked)?;
        tokio::task::spawn_blocking(move || owner.join().map_err(|_| StoreError::OwnerPanicked))
            .await
            .map_err(|_| StoreError::OwnerPanicked)??;
        Ok(())
    }
}

impl StoreReservation {
    /// Open the reserved database using the matching configuration.
    ///
    /// Consuming the reservation ensures that a failed open or path mismatch
    /// releases the process lock instead of leaving a partially initialized
    /// reservation alive.
    pub fn open(self, config: StoreConfig) -> Result<Store, StoreError> {
        let StoreReservation { path, lock } = self;
        validate_config(&config)?;
        if path.as_path() != config.path.as_path() {
            return Err(StoreError::InvalidConfiguration(
                "store reservation path does not match store configuration path",
            ));
        }
        Store::open_reserved(config, lock)
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        if self.owner.is_some() {
            let (reply, _response) = oneshot::channel();
            let _ = self.handle.try_send(Command::Shutdown { reply }, 1);
        }
    }
}

impl StoreHandle {
    /// Return the Host identity bound to this store.
    #[must_use]
    pub const fn host_id(&self) -> PeerId {
        self.host_id
    }

    /// Claim the sole live writer for one execution identity.
    ///
    /// Claims are process-local and shared by every clone of this handle. The
    /// returned capability is intentionally not `Clone`; dropping it releases
    /// the claim and permits a later owner to resume the execution.
    pub fn claim_execution(&self, execution_id: ExecId) -> Result<ExecutionStore, StoreError> {
        let mut claims = self
            .execution_claims
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !claims.insert(execution_id) {
            return Err(StoreError::ExecutionAlreadyClaimed(execution_id));
        }
        Ok(ExecutionStore {
            handle: self.clone(),
            execution_id,
        })
    }

    /// Load one durable admission root, including any terminal failure.
    pub async fn load_execution_request(
        &self,
        execution_id: ExecId,
    ) -> Result<Option<ExecutionRequest>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(
            Command::LoadExecutionRequest {
                execution_id,
                reply,
            },
            128,
        )
        .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Bind an open Join request to one exact offer before local consent is
    /// signed. The store performs a compare-and-set: retries can observe the
    /// same target, but they cannot replace it with another negotiation.
    pub async fn bind_join_target(
        &self,
        execution_id: ExecId,
        target: NegotiationTarget,
    ) -> Result<AdmissionBindingOutcome, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(
            Command::BindJoinTarget {
                execution_id,
                target,
                reply,
            },
            256,
        )
        .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// List bounded admission roots in their durable creation order.
    pub async fn list_execution_requests(
        &self,
        limit: usize,
    ) -> Result<Vec<ExecutionRequest>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(
            Command::ListExecutionRequests { limit, reply },
            self.command_cost(256, limit, 512)?,
        )
        .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// List one ordered page of every durable request that can still make
    /// progress after restart.
    ///
    /// The store filters terminal request and execution facts before applying
    /// the limit. This keeps terminal history from consuming a page and
    /// hiding a later nonterminal request. The page contains request roots and
    /// fixed-size activation/execution row metadata only. A prepared activation
    /// remains in the projection so the daemon can resume its private
    /// negotiation; its bounded payload is loaded separately.
    pub async fn list_recovery_candidates(
        &self,
        cursor: RecoveryCursor,
        limit: usize,
    ) -> Result<RecoveryPage, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(
            Command::ListRecoveryCandidates {
                cursor,
                limit,
                reply,
            },
            self.command_cost(512, limit, 512)?,
        )
        .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Import one bounded Wasm program under its verified content address.
    pub async fn register_program(
        &self,
        wasm: Vec<u8>,
        now_ms: u64,
    ) -> Result<(ProgramHash, ProgramStoreOutcome), StoreError> {
        let max = usize::try_from(arena0_program::PROGRAM_MAX_LEN).map_err(|_| {
            StoreError::InvalidConfiguration("program size bound does not fit usize")
        })?;
        if wasm.is_empty() || wasm.len() > max {
            return Err(StoreError::CommandTooLarge {
                required: wasm.len(),
                capacity: max,
            });
        }
        let hash = ProgramHash::of(&wasm);
        let cost = self.command_cost(wasm.len(), 1, 512)?;
        let (reply, response) = oneshot::channel();
        self.send(
            Command::RegisterProgram {
                hash,
                wasm,
                now_ms,
                reply,
            },
            cost,
        )
        .await?;
        Ok((hash, response.await.map_err(|_| StoreError::ReplyDropped)??))
    }

    /// Load one exact content-addressed Wasm program.
    pub async fn load_program(
        &self,
        hash: ProgramHash,
    ) -> Result<Option<StoredProgram>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::LoadProgram { hash, reply }, 128).await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// List bounded content addresses for registry recovery.
    pub async fn list_programs(&self, limit: usize) -> Result<Vec<ProgramHash>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(
            Command::ListPrograms { limit, reply },
            self.command_cost(128, limit, 40)?,
        )
        .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Unregister a program from new admission while retaining its bytes for
    /// existing durable execution recovery.
    pub async fn remove_program(
        &self,
        hash: ProgramHash,
        now_ms: u64,
    ) -> Result<ProgramRemoveOutcome, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(
            Command::RemoveProgram {
                hash,
                now_ms,
                reply,
            },
            128,
        )
        .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Load a permanent activation record.
    pub async fn load_activation(
        &self,
        execution_id: ExecId,
    ) -> Result<Option<ActivationRecord>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(
            Command::LoadActivation {
                execution_id,
                reply,
            },
            64,
        )
        .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Load and validate one execution aggregate.
    pub async fn load_execution(
        &self,
        execution_id: ExecId,
    ) -> Result<Option<ExecutionState>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(
            Command::LoadExecution {
                execution_id,
                reply,
            },
            64,
        )
        .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Load the unique local execution bound to a session identity.
    pub async fn load_execution_by_session(
        &self,
        session_id: SessionHash,
    ) -> Result<Option<ExecutionState>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::LoadExecutionBySession { session_id, reply }, 64)
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// List bounded permanent activation records for restart recovery.
    pub async fn list_activations(
        &self,
        limit: usize,
    ) -> Result<Vec<ActivationRecord>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(
            Command::ListActivations { limit, reply },
            self.command_cost(128, limit, 256)?,
        )
        .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// List bounded execution aggregates for restart recovery.
    pub async fn list_executions(&self, limit: usize) -> Result<Vec<ExecutionState>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(
            Command::ListExecutions { limit, reply },
            self.command_cost(128, limit, 1024)?,
        )
        .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// List accepted frames that still need resolution after a restart. The
    /// returned frame is the canonical durable copy.
    pub async fn list_pending_inbox(
        &self,
        execution_id: ExecId,
        limit: usize,
    ) -> Result<Vec<PendingInboxItem>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(
            Command::ListPendingInbox {
                execution_id,
                limit,
                reply,
            },
            self.command_cost(256, limit, 1_024)?,
        )
        .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Load the exact pending agent request for one execution.
    ///
    /// This is a read projection and does not claim the execution writer. It
    /// is available on the cloneable handle because the daemon's API and
    /// supervisor observe an actor's durable continuation while the actor
    /// owns the non-clone [`ExecutionStore`].
    pub async fn list_pending_requests(
        &self,
        execution_id: ExecId,
    ) -> Result<Vec<PendingRequest>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(
            Command::ListPendingRequests {
                execution_id,
                reply,
            },
            512,
        )
        .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Read a bounded public trace range from the durable execution.
    ///
    /// The owner validates the complete trace before selecting the requested
    /// range. A malformed or gapped prefix therefore cannot become a
    /// plausible daemon event projection.
    pub async fn read_trace(
        &self,
        execution_id: ExecId,
        from: u64,
        to: u64,
    ) -> Result<Vec<arena0_protocol::TraceEntry>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(
            Command::ReadTrace {
                execution_id,
                from,
                to,
                reply,
            },
            self.command_cost(256, 1, 1_024)?,
        )
        .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Read a bounded projection of durable private handler commits.
    ///
    /// The store validates the selected durable records and their coordinates.
    /// Full-projection validation remains part of execution recovery. Returned
    /// values contain only event/effect kinds, payload sizes, fuel, and durable
    /// coordinates; raw private payloads and replacement state never cross
    /// this capability boundary.
    pub async fn read_private_summaries(
        &self,
        execution_id: ExecId,
        from: Option<u64>,
        limit: usize,
    ) -> Result<PrivateInspectionPage, StoreError> {
        if limit > MAX_PRIVATE_INSPECTION_RECORDS {
            return Err(StoreError::InvalidConfiguration(
                "private inspection limit exceeds the fixed bound",
            ));
        }
        if limit == 0 {
            return Err(StoreError::InvalidConfiguration(
                "private inspection limit must be non-zero",
            ));
        }
        let (reply, response) = oneshot::channel();
        self.send(
            Command::ReadPrivateSummaries {
                execution_id,
                from,
                limit,
                reply,
            },
            self.command_cost(512, limit, 512)?,
        )
        .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Import one validated portable receipt as foreign evidence.
    ///
    /// The artifact is persisted without an execution binding. Repeating the
    /// operation with the exact same receipt is idempotent; a different
    /// artifact for an occupied receipt key is rejected.
    pub async fn import_receipt(
        &self,
        receipt: ReceiptArtifact,
        now_ms: u64,
    ) -> Result<ReceiptImportOutcome, StoreError> {
        let encoded = receipt.encode()?;
        let cost = self.command_cost(encoded.len(), 1, 512)?;
        let (reply, response) = oneshot::channel();
        self.send(
            Command::ImportReceipt {
                receipt: Box::new(receipt),
                now_ms,
                reply,
            },
            cost,
        )
        .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Load this Host's own publication for a session.
    pub async fn load_receipt(
        &self,
        session_id: SessionHash,
    ) -> Result<Option<StoredReceipt>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::LoadReceipt { session_id, reply }, 128)
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Load one receipt by its content-addressed identity.
    pub async fn load_receipt_by_id(
        &self,
        receipt_id: ReceiptId,
    ) -> Result<Option<StoredReceipt>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::LoadReceiptById { receipt_id, reply }, 128)
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// List receipts in deterministic key order.
    pub async fn list_receipts(&self, limit: usize) -> Result<Vec<StoredReceipt>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(
            Command::ListReceipts { limit, reply },
            self.command_cost(256, limit, 1_024)?,
        )
        .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Load the optional durable Host user agent.
    pub async fn load_user_agent(&self) -> Result<Option<String>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.send(Command::LoadUserAgent { reply }, 128).await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Persist the Host user agent in the store metadata.
    pub async fn set_user_agent(&self, value: String) -> Result<(), StoreError> {
        validate_user_agent(&value)?;
        let cost = self.command_cost(value.len(), 1, USER_AGENT_COMMAND_OVERHEAD)?;
        let (reply, response) = oneshot::channel();
        self.send(Command::SetUserAgent { value, reply }, cost)
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    async fn send(&self, command: Command, required: usize) -> Result<(), StoreError> {
        let required = required.max(1);
        if required > self.queue_bytes {
            return Err(StoreError::CommandTooLarge {
                required,
                capacity: self.queue_bytes,
            });
        }
        let permits = u32::try_from(required).map_err(|_| StoreError::CommandTooLarge {
            required,
            capacity: self.queue_bytes,
        })?;
        let budget = Arc::clone(&self.budget)
            .acquire_many_owned(permits)
            .await
            .map_err(|_| StoreError::Closed)?;
        self.sender
            .send(QueuedCommand {
                command,
                _budget: budget,
            })
            .await
            .map_err(|_| StoreError::Closed)
    }

    fn command_cost(
        &self,
        base: usize,
        count: usize,
        per_item: usize,
    ) -> Result<usize, StoreError> {
        let items = count
            .checked_mul(per_item)
            .ok_or(StoreError::CommandTooLarge {
                required: usize::MAX,
                capacity: self.queue_bytes,
            })?;
        base.checked_add(items).ok_or(StoreError::CommandTooLarge {
            required: usize::MAX,
            capacity: self.queue_bytes,
        })
    }

    fn try_send(&self, command: Command, required: usize) -> Result<(), StoreError> {
        let required = required.max(1);
        if required > self.queue_bytes {
            return Err(StoreError::CommandTooLarge {
                required,
                capacity: self.queue_bytes,
            });
        }
        let permits = u32::try_from(required).map_err(|_| StoreError::CommandTooLarge {
            required,
            capacity: self.queue_bytes,
        })?;
        let budget = self
            .budget
            .clone()
            .try_acquire_many_owned(permits)
            .map_err(|_| StoreError::Closed)?;
        self.sender
            .try_send(QueuedCommand {
                command,
                _budget: budget,
            })
            .map_err(|_| StoreError::Closed)
    }
}

impl Drop for ExecutionStore {
    fn drop(&mut self) {
        let mut claims = self
            .handle
            .execution_claims
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        claims.remove(&self.execution_id);
    }
}

impl ExecutionStore {
    /// Return the execution identity fixed by this writer capability.
    #[must_use]
    pub const fn execution_id(&self) -> ExecId {
        self.execution_id
    }

    /// Bind an open Join request to its selected offer before ticket signing.
    pub async fn bind_join_target(
        &mut self,
        target: NegotiationTarget,
    ) -> Result<AdmissionBindingOutcome, StoreError> {
        self.handle
            .bind_join_target(self.execution_id, target)
            .await
    }

    /// Create the durable local admission root for this execution.
    ///
    /// Admission is the first lifecycle mutation. Requiring the already
    /// claimed writer here means request creation, negotiation, activation,
    /// and execution all share one live ownership token. Explicit admission
    /// requires `Some(params)`; join admission may omit its preferred params
    /// until the creator's offer is authenticated.
    pub async fn create_execution_request(
        &mut self,
        program_hash: ProgramHash,
        params: Option<JsonBytes>,
        admission: ExecutionAdmission,
        created_at_ms: u64,
    ) -> Result<ExecutionRequestOutcome, StoreError> {
        let params_len = params.as_ref().map_or(0, JsonBytes::len);
        if params_len > arena0_protocol::MAX_PARAMS_LEN {
            return Err(StoreError::CommandTooLarge {
                required: params_len,
                capacity: arena0_protocol::MAX_PARAMS_LEN,
            });
        }
        let cost = self.handle.command_cost(params_len, 1, 512)?;
        let (reply, response) = oneshot::channel();
        self.handle
            .send(
                Command::CreateExecutionRequest {
                    execution_id: self.execution_id,
                    program_hash,
                    params,
                    admission,
                    created_at_ms,
                    reply,
                },
                cost,
            )
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Load this execution's durable admission root.
    pub async fn load_execution_request(&self) -> Result<Option<ExecutionRequest>, StoreError> {
        self.handle.load_execution_request(self.execution_id).await
    }

    /// Record a bounded failure on this execution's admission root.
    pub async fn record_execution_request_failure(
        &mut self,
        reason: impl Into<String>,
    ) -> Result<ExecutionRequestFailureOutcome, StoreError> {
        let reason = bounded_reason(reason.into())?;
        let (reply, response) = oneshot::channel();
        self.handle
            .send(
                Command::RecordExecutionRequestFailure {
                    execution_id: self.execution_id,
                    reason,
                    reply,
                },
                256,
            )
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Load the per-execution local secret, creating it durably on first use.
    pub async fn load_or_create_execution_salt(
        &mut self,
        now_ms: u64,
    ) -> Result<ExecutionSalt, StoreError> {
        let (reply, response) = oneshot::channel();
        self.handle
            .send(
                Command::LoadOrCreateExecutionSalt {
                    execution_id: self.execution_id,
                    now_ms,
                    reply,
                },
                128,
            )
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Prepare a validated activation under this execution's permanent key.
    pub async fn prepare_activation(
        &mut self,
        prepared: PreparedActivation,
        now_ms: u64,
    ) -> Result<PrepareActivationOutcome, StoreError> {
        prepared.validate().map_err(|error| {
            StoreError::Corruption(format!("prepared activation validation failed: {error}"))
        })?;
        let cost = self
            .handle
            .command_cost(encoded_len(&prepared, MAX_FRAME_BYTES)?, 1, 256)?;
        let (reply, response) = oneshot::channel();
        self.handle
            .send(
                Command::PrepareActivation {
                    execution_id: self.execution_id,
                    prepared,
                    now_ms,
                    reply,
                },
                cost,
            )
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Compare-and-set this execution's prepared activation to its permanent
    /// committed state.
    pub async fn commit_activation(
        &mut self,
        activation: Activation,
        now_ms: u64,
    ) -> Result<CommitActivationOutcome, StoreError> {
        activation.validate().map_err(|error| {
            StoreError::Corruption(format!("activation validation failed: {error}"))
        })?;
        let cost = self
            .handle
            .command_cost(encoded_len(&activation, MAX_FRAME_BYTES)?, 1, 256)?;
        let (reply, response) = oneshot::channel();
        self.handle
            .send(
                Command::CommitActivation {
                    execution_id: self.execution_id,
                    activation,
                    now_ms,
                    reply,
                },
                cost,
            )
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Load this execution's permanent activation record.
    pub async fn load_activation(&self) -> Result<Option<ActivationRecord>, StoreError> {
        self.handle.load_activation(self.execution_id).await
    }

    /// Load and validate this execution's aggregate.
    pub async fn load_execution(&self) -> Result<Option<ExecutionState>, StoreError> {
        self.handle.load_execution(self.execution_id).await
    }

    /// List this execution's accepted frames that still need resolution.
    pub async fn list_pending_inbox(
        &self,
        limit: usize,
    ) -> Result<Vec<PendingInboxItem>, StoreError> {
        self.handle
            .list_pending_inbox(self.execution_id, limit)
            .await
    }

    /// Load the current agent-facing request, including its original durable
    /// payload. Acknowledged request outbox rows are retained specifically so
    /// restart recovery can re-emit a request that was delivered before the
    /// consumer submitted its answer.
    pub async fn pending_requests(&self) -> Result<Vec<PendingRequest>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.handle
            .send(
                Command::ListPendingRequests {
                    execution_id: self.execution_id,
                    reply,
                },
                512,
            )
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Insert the initial execution aggregate from typed genesis inputs.
    ///
    /// The execution id comes from this capability, and the protocol
    /// constructor enforces version-zero `Activating` state with no proposal,
    /// timers, or private progress. There is no API that accepts an arbitrary
    /// post-genesis [`ExecutionState`] for insertion.
    pub async fn create_execution(
        &mut self,
        activation: Activation,
        producer: PeerId,
        shared_state: SharedStateBytes,
        local_state: LocalStateBytes,
        now_ms: u64,
    ) -> Result<CreateExecutionOutcome, StoreError> {
        let genesis = ExecutionState::new(
            self.execution_id,
            activation.clone(),
            producer,
            shared_state.clone(),
            local_state.clone(),
        )?;
        let cost = self
            .handle
            .command_cost(state_bytes(&genesis)?.len(), 1, 512)?;
        let (reply, response) = oneshot::channel();
        self.handle
            .send(
                Command::CreateExecution {
                    execution_id: self.execution_id,
                    activation,
                    producer,
                    shared_state,
                    local_state,
                    now_ms,
                    reply,
                },
                cost,
            )
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Derive and atomically apply one pure protocol input to this execution.
    pub async fn apply_input(
        &mut self,
        input: ExecutionInput,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        if matches!(&input, ExecutionInput::ReceiptBody(_)) {
            return Err(StoreError::ReceiptBodyRequiresAssembly);
        }
        self.apply_input_inner(input, None, now_ms).await
    }

    async fn apply_input_inner(
        &mut self,
        input: ExecutionInput,
        inbox: Option<InboxReference>,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        let cost = self
            .handle
            .command_cost(input_bytes(&input)?.len(), 1, 1_024)?;
        let (reply, response) = oneshot::channel();
        self.handle
            .send(
                Command::ApplyInput {
                    execution_id: self.execution_id,
                    input: Box::new(input),
                    inbox,
                    now_ms,
                    reply,
                },
                cost,
            )
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Accept one frame whose source is the identity authenticated by the
    /// transport. The returned outcome is the durable acknowledgement point.
    pub async fn accept_inbound(
        &mut self,
        authenticated_source: PeerId,
        frame: ExecFrame,
        now_ms: u64,
    ) -> Result<InboxAcceptOutcome, StoreError> {
        let frame = AuthenticatedFrame {
            source: authenticated_source,
            frame,
        };
        let cost = self
            .handle
            .command_cost(frame_bytes(&frame)?.len(), 1, 512)?;
        let (reply, response) = oneshot::channel();
        self.handle
            .send(
                Command::AcceptInbound {
                    execution_id: self.execution_id,
                    frame: Box::new(frame),
                    now_ms,
                    reply,
                },
                cost,
            )
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Resolve an accepted signature, terminal signature, or abort frame.
    pub async fn apply_inbound(
        &mut self,
        inbox_id: InboxId,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        let (reply, response) = oneshot::channel();
        self.handle
            .send(
                Command::ApplyInbound {
                    execution_id: self.execution_id,
                    inbox_id,
                    now_ms,
                    reply,
                },
                256,
            )
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Resolve an accepted message with its guest-produced shared delta.
    pub async fn apply_inbound_message(
        &mut self,
        inbox_id: InboxId,
        delta: SharedDelta,
        now_ms: u64,
    ) -> Result<ApplyOutcome, StoreError> {
        let input = ExecutionInput::ProposeShared(delta.clone());
        let cost = self
            .handle
            .command_cost(input_bytes(&input)?.len(), 1, 1_024)?;
        let (reply, response) = oneshot::channel();
        self.handle
            .send(
                Command::ApplyInboundMessage {
                    execution_id: self.execution_id,
                    inbox_id,
                    delta: Box::new(delta),
                    now_ms,
                    reply,
                },
                cost,
            )
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Explicitly reject an accepted inbound frame that cannot become valid.
    pub async fn reject_inbound(
        &mut self,
        inbox_id: InboxId,
        now_ms: u64,
    ) -> Result<InboxRejectOutcome, StoreError> {
        let (reply, response) = oneshot::channel();
        self.handle
            .send(
                Command::RejectInbound {
                    execution_id: self.execution_id,
                    inbox_id,
                    now_ms,
                    reply,
                },
                128,
            )
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Lease the earliest ready outbox occurrence for this execution.
    pub async fn lease_next_outbox(
        &mut self,
        now_ms: u64,
    ) -> Result<Option<LeasedOutbox>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.handle
            .send(
                Command::LeaseOutbox {
                    execution_id: self.execution_id,
                    now_ms,
                    reply,
                },
                640,
            )
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Acknowledge a leased outbox occurrence for this execution.
    pub async fn acknowledge_outbox(
        &mut self,
        outbox_id: OutboxId,
        lease_id: LeaseId,
    ) -> Result<OutboxDeliveryOutcome, StoreError> {
        let (reply, response) = oneshot::channel();
        self.handle
            .send(
                Command::AcknowledgeOutbox {
                    execution_id: self.execution_id,
                    outbox_id,
                    lease_id,
                    reply,
                },
                256,
            )
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Return a leased occurrence to pending with the configured delay.
    pub async fn retry_outbox(
        &mut self,
        outbox_id: OutboxId,
        lease_id: LeaseId,
        now_ms: u64,
        reason: impl Into<String>,
    ) -> Result<OutboxDeliveryOutcome, StoreError> {
        let reason = bounded_reason(reason.into())?;
        let (reply, response) = oneshot::channel();
        self.handle
            .send(
                Command::RetryOutbox {
                    execution_id: self.execution_id,
                    outbox_id,
                    lease_id,
                    now_ms,
                    reason,
                    reply,
                },
                512,
            )
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Recover this execution's expired outbox leases.
    pub async fn recover_expired_leases(
        &mut self,
        now_ms: u64,
    ) -> Result<RecoveryReport, StoreError> {
        let (reply, response) = oneshot::channel();
        self.handle
            .send(
                Command::RecoverExpiredLeases {
                    execution_id: self.execution_id,
                    now_ms,
                    reply,
                },
                128,
            )
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// List due active timers for this execution.
    pub async fn due_timers(
        &self,
        now_ms: u64,
        limit: usize,
    ) -> Result<Vec<ActiveTimer>, StoreError> {
        let (reply, response) = oneshot::channel();
        self.handle
            .send(
                Command::DueTimers {
                    execution_id: self.execution_id,
                    now_ms,
                    limit,
                    reply,
                },
                self.handle.command_cost(256, limit, 256)?,
            )
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }

    /// Assemble the exact receipt body from authoritative rows and stage it
    /// through the protocol reducer in one SQLite transaction.
    pub async fn assemble_receipt(&mut self, now_ms: u64) -> Result<ApplyOutcome, StoreError> {
        let (reply, response) = oneshot::channel();
        self.handle
            .send(
                Command::AssembleReceipt {
                    execution_id: self.execution_id,
                    now_ms,
                    reply,
                },
                512,
            )
            .await?;
        response.await.map_err(|_| StoreError::ReplyDropped)?
    }
}

fn validate_config(config: &StoreConfig) -> Result<(), StoreError> {
    if config.queue_capacity == 0 {
        return Err(StoreError::InvalidConfiguration(
            "queue capacity must be greater than zero",
        ));
    }
    if config.queue_bytes == 0 {
        return Err(StoreError::InvalidConfiguration(
            "queue byte capacity must be greater than zero",
        ));
    }
    if config.queue_bytes > u32::MAX as usize {
        return Err(StoreError::InvalidConfiguration(
            "queue byte capacity must fit a semaphore permit count",
        ));
    }
    Ok(())
}

/// Validate a Host user agent before it is persisted or used for filesystem
/// initialization.
pub fn validate_user_agent(value: &str) -> Result<(), StoreError> {
    if value.is_empty() {
        return Err(StoreError::InvalidConfiguration(
            "user agent must contain at least one UTF-8 byte",
        ));
    }
    if value.len() > MAX_USER_AGENT_BYTES {
        return Err(StoreError::InvalidConfiguration(
            "user agent must be at most 256 UTF-8 bytes",
        ));
    }
    if value.chars().all(char::is_whitespace) {
        return Err(StoreError::InvalidConfiguration(
            "user agent must contain a non-whitespace character",
        ));
    }
    if value.chars().any(char::is_control) {
        return Err(StoreError::InvalidConfiguration(
            "user agent must not contain control characters",
        ));
    }
    Ok(())
}

fn decode_user_agent(bytes: Vec<u8>) -> Result<String, StoreError> {
    let value = String::from_utf8(bytes).map_err(|error| {
        StoreError::Corruption(format!("stored user agent is not UTF-8: {error}"))
    })?;
    validate_user_agent(&value).map_err(|error| {
        StoreError::Corruption(format!("stored user agent is invalid: {error}"))
    })?;
    Ok(value)
}

#[cfg(test)]
mod tests;
