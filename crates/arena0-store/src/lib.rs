//! Authoritative per-Host SQLite persistence.
//!
//! [`Store`] owns one SQLite connection behind a mutex. [`StoreHandle`]
//! is a cloneable asynchronous capability for registry and receipt-artifact
//! operations plus read projections. Operations run on the blocking pool and
//! serialize access to the connection.
//!
//! The store owns the SQLite transaction boundaries around flat event
//! dispatches, shared proposals, signatures, terminal publication, and the
//! recovery projections needed for scheduling. Protocol constructors remain
//! authoritative for state and certificate validation.

use arena0_protocol::{Effect, Event};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use arena0_crypto::ExecutionSalt;
use arena0_program::{JsonBytes, ProgramHash};
use arena0_protocol::execution::{
    ExecutionState, ExecutionStatus, ExecutionVersion, ReceiptArtifact, ReceiptId, TimerId,
};
use arena0_protocol::{
    Activation, ExecId, ExecLifecycle, ExecutionAdmission, LocalStateBytes, NegotiationTarget,
    PeerId, PreparedActivation, ProtocolError, SessionHash, SharedStateBytes, TimerPayload,
};
use borsh::{BorshDeserialize, BorshSerialize};
use rusqlite::{Connection, OptionalExtension, params};
use thiserror::Error;

mod codec;
mod database;
mod lock;
use codec::*;
use database::Database;
use lock::{
    OwnerLock, acquire_process_lock, configure_connection, initialize_schema, prepare_database_file,
};

const SCHEMA_VERSION: u64 = 7;
const ENVELOPE_VERSION: u16 = 2;
const ENVELOPE_MAGIC: [u8; 8] = *b"AR0STOR1";
const ENVELOPE_DOMAIN: &[u8] = b"arena0/store-envelope/v2";
const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_ERROR_BYTES: usize = 4 * 1024;
const MAX_ADMISSION_BYTES: usize = 4 * 1024;
const MAX_ACTIVATION_BYTES: usize = 16 * 1024 * 1024;
// Timer rows store the complete `TimerPayload` (including the type name), not
// just its value bytes. Keep the envelope bound large enough for both protocol
// components plus their Borsh length prefixes.
const MAX_TIMER_RECORD_BYTES: usize =
    arena0_protocol::MAX_TIMER_PAYLOAD_BYTES + arena0_protocol::MAX_TERMINAL_REASON_BYTES + 16;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const MAX_ENVELOPE_OVERHEAD: usize = 8 + 2 + 2 + 4 + 32;
const MAX_USER_AGENT_BYTES: usize = 256;

/// Maximum number of local event summaries returned by one inspection page.
/// The durable store may contain more records; callers use the returned cursor
/// to request another bounded page.
pub const MAX_EVENT_INSPECTION_RECORDS: usize = 256;

/// Configuration for one Host-owned SQLite database.
#[derive(Debug, Clone)]
pub struct StoreConfig {
    path: PathBuf,
    host_id: PeerId,
    busy_timeout: Duration,
}

impl StoreConfig {
    /// Bind a store to a database path and the owning Host identity.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>, host_id: PeerId) -> Self {
        Self {
            path: path.into(),
            host_id,
            busy_timeout: DEFAULT_BUSY_TIMEOUT,
        }
    }
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
    /// Active catalog membership was removed; artifact bytes remain durable.
    Removed,
    /// It was already absent from active catalog membership.
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

/// One execution with unfinished protocol, proof, or final-frame delivery work and the
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

    /// Return the requested program content address.
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
    /// The store connection has closed or is poisoned.
    #[error("store is closed")]
    Closed,
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
    /// The database contains malformed, tampered, or internally inconsistent data.
    #[error("store corruption: {0}")]
    Corruption(String),
    /// A protocol value failed its construction or validation invariant.
    #[error("protocol error: {0}")]
    Protocol(#[from] ProtocolError),
    /// An operation's payload exceeds its size bound.
    #[error("store payload requires {required} bytes, limit is {capacity}")]
    PayloadTooLarge { required: usize, capacity: usize },
    /// A store configuration is invalid.
    #[error("invalid store configuration: {0}")]
    InvalidConfiguration(&'static str),
    /// A local admission request does not authorize the proposed activation.
    #[error("invalid execution admission: {0}")]
    InvalidAdmission(String),
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

/// The event kind in one Host-local event record. Payloads remain opaque; this
/// enum is the store's safe diagnostic projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    SessionStarted,
    MessageReceived,
    InputReceived,
    TimerFired,
}

/// One effect's kind and bounded payload size. The store never returns
/// the effect payload itself from inspection reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EffectSummary {
    pub kind: EffectKind,
    pub payload_bytes: Option<usize>,
}

/// The effect kind in one Host-local event record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffectKind {
    SessionEnd,
    SessionAbort,
    Broadcast,
    SetTimer,
    Fail,
}

/// A safe projection of one durable event record for local diagnostics.
/// Event position is an authoritative local coordinate. An event may produce
/// more than one agreed step (an authored message stages a proposal after its
/// local dispatch), so the relation is represented as a list rather than a
/// misleading scalar. Payloads contain only kinds and sizes, never raw private
/// values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRecordSummary {
    pub event_position: u64,
    pub agreed_steps: Vec<u64>,
    pub event: EventKind,
    pub input_payload_bytes: Option<usize>,
    pub effects: Vec<EffectSummary>,
}

/// One bounded page of local event summaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventInspectionPage {
    from: u64,
    summaries: Vec<EventRecordSummary>,
    total: u64,
    next: Option<u64>,
}

impl EventInspectionPage {
    /// Return the first event position represented by this page.
    #[must_use]
    pub const fn from(&self) -> u64 {
        self.from
    }

    /// Borrow summaries in ascending event-position order.
    #[must_use]
    pub fn summaries(&self) -> &[EventRecordSummary] {
        &self.summaries
    }

    /// Consume the page and return its summaries.
    #[must_use]
    pub fn into_summaries(self) -> Vec<EventRecordSummary> {
        self.summaries
    }

    /// Return the durable number of local event records at read time.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// Return the next event position when another page remains.
    #[must_use]
    pub const fn next(&self) -> Option<u64> {
        self.next
    }

    pub(crate) const fn new(
        from: u64,
        summaries: Vec<EventRecordSummary>,
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

impl EventRecordSummary {
    fn from_record(
        event_position: u64,
        agreed_steps: Vec<u64>,
        event: &Event<Vec<u8>>,
        effects: &[Effect],
    ) -> Self {
        let (event_kind, input_payload_bytes) = match event {
            Event::SessionStarted { .. } => (EventKind::SessionStarted, None),
            Event::MessageReceived { msg, .. } => (EventKind::MessageReceived, Some(msg.len())),
            Event::InputReceived { data, .. } => (EventKind::InputReceived, Some(data.len())),
            Event::TimerFired { timer } => (EventKind::TimerFired, Some(timer.data.len())),
        };
        let effects = effects
            .iter()
            .map(|effect| match effect {
                Effect::SessionEnd { outcome } => EffectSummary {
                    kind: EffectKind::SessionEnd,
                    payload_bytes: Some(outcome.len()),
                },
                Effect::SessionAbort { reason } => EffectSummary {
                    kind: EffectKind::SessionAbort,
                    payload_bytes: Some(reason.len()),
                },
                Effect::Broadcast { data } => EffectSummary {
                    kind: EffectKind::Broadcast,
                    payload_bytes: Some(data.len()),
                },
                Effect::SetTimer { timer, .. } => EffectSummary {
                    kind: EffectKind::SetTimer,
                    payload_bytes: Some(timer.data.len()),
                },
                Effect::Fail { reason } => EffectSummary {
                    kind: EffectKind::Fail,
                    payload_bytes: Some(reason.len()),
                },
            })
            .collect();
        Self {
            event_position,
            agreed_steps,
            event: event_kind,
            input_payload_bytes,
            effects,
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

/// One actor-computed transition and its atomic durable consequences.
///
/// The execution's sole writer computes `next` with a protocol transition.
/// `change` must describe that same transition, including its consumed timer. The store persists the result and side rows;
/// it does not execute the protocol transition again.
#[derive(Debug)]
pub struct TransitionRecord {
    /// Version from which the actor computed the transition.
    pub expected: ExecutionVersion,
    /// Complete resulting execution state.
    pub next: ExecutionState,
    /// Side rows to persist with the state.
    pub change: Change,
    /// Local persistence time in milliseconds.
    pub now_ms: u64,
}

/// Durable consequences of a protocol transition computed by the actor.
#[derive(Debug)]
pub enum Change {
    /// Persist only the execution state: activation, an authenticated stop,
    /// an end-confirmation phase, or dropping a rejected outgoing message
    /// write no rows beyond it.
    State,
    /// Record an accepted guest dispatch and consume its durable source.
    Dispatch {
        event: Event<Vec<u8>>,
        effects: Vec<Effect>,
        timer_id: Option<TimerId>,
    },
    /// Record a signature, releasing a certified proposal when present.
    StepSignature {
        certified: Option<arena0_protocol::SharedProposal>,
    },
    /// Publish evidence assembled from durable rows.
    Publish { artifact: ReceiptArtifact },
}

/// One active timer scheduling projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveTimer {
    /// Timer identity.
    pub timer_id: TimerId,
    /// Absolute due time.
    pub deadline_ms: u64,
    /// Exact typed timer payload emitted by the program.
    pub timer: TimerPayload,
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

/// Cloneable asynchronous capability for one store connection.
///
/// This handle owns bounded registry and receipt-artifact operations plus read
/// projections.
/// Execution mutations require an [`ExecutionStore`] claimed for the target
/// execution, so cloning this handle cannot create a second live writer.
///
/// Operations run on the blocking pool and serialize on the connection mutex.
/// Dropping an operation's future does not cancel work already submitted to
/// that pool; a lost response must be treated as an unknown outcome.
#[derive(Clone, Debug)]
pub struct StoreHandle {
    inner: Arc<Inner>,
}

struct Inner {
    db: StdMutex<Option<Database>>,
    execution_claims: StdMutex<HashSet<ExecId>>,
    host_id: PeerId,
}

impl std::fmt::Debug for Inner {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Inner")
            .field("host_id", &self.host_id)
            .finish_non_exhaustive()
    }
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
///     record: arena0_store::TransitionRecord,
/// ) {
///     let _future = writer.persist(record);
/// }
/// ```
///
/// A cloneable [`StoreHandle`] exposes read and registry capabilities only;
/// execution mutations, including Join target binding, require the
/// non-cloneable [`ExecutionStore`]:
///
/// ```compile_fail,E0624
/// fn shared_handle_cannot_bind_join_target(
///     handle: &arena0_store::StoreHandle,
///     execution_id: arena0_protocol::ExecId,
///     target: arena0_protocol::NegotiationTarget,
/// ) {
///     let _future = handle.bind_join_target(execution_id, target);
/// }
/// ```
#[derive(Debug)]
pub struct ExecutionStore {
    handle: StoreHandle,
    execution_id: ExecId,
}

/// The owner of one SQLite connection and its process lock.
pub struct Store {
    handle: StoreHandle,
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
        formatter.debug_struct("Store").finish_non_exhaustive()
    }
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

    /// Open a database, acquire its process lock, and validate it.
    pub fn open(config: StoreConfig) -> Result<Self, StoreError> {
        Self::reserve(&config.path)?.open(config)
    }

    fn open_reserved(config: StoreConfig, lock: OwnerLock) -> Result<Self, StoreError> {
        let db = Database::open(&config, lock)?;
        let handle = StoreHandle {
            inner: Arc::new(Inner {
                db: StdMutex::new(Some(db)),
                execution_claims: StdMutex::new(HashSet::new()),
                host_id: config.host_id,
            }),
        };
        Ok(Self { handle })
    }

    /// Borrow the cloneable store handle.
    #[must_use]
    pub const fn handle(&self) -> &StoreHandle {
        &self.handle
    }

    /// Close the connection and release the process lock.
    ///
    /// This waits for any operation holding the connection mutex. Submitted
    /// operations that acquire the mutex after closure fail without running.
    /// Calls through retained handles then return [`StoreError::Closed`].
    pub async fn shutdown(self) -> Result<(), StoreError> {
        let inner = Arc::clone(&self.handle.inner);
        tokio::task::spawn_blocking(move || {
            inner
                .db
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take();
        })
        .await
        .map_err(|_| StoreError::Closed)
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
        self.handle
            .inner
            .db
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }
}

impl StoreHandle {
    /// Read bounded reconstruction metadata for a dormant end handshake.
    pub async fn end_wake_candidate(
        &self,
        execution_id: ExecId,
    ) -> Result<Option<RecoveryCandidate>, StoreError> {
        self.run(move |db| db.end_wake_candidate(execution_id))
            .await
    }
    /// Read terminal routing metadata without decoding execution state or
    /// memory images. The phase is independent of receipt publication.
    pub async fn execution_end(
        &self,
        session_id: SessionHash,
    ) -> Result<Option<(ExecId, arena0_protocol::EndPhase)>, StoreError> {
        self.run(move |db| db.execution_end(session_id)).await
    }

    /// Return the Host identity bound to this store.
    #[must_use]
    pub fn host_id(&self) -> PeerId {
        self.inner.host_id
    }

    /// Claim the sole live writer for one execution identity.
    ///
    /// Claims are process-local and shared by every clone of this handle. The
    /// returned capability is intentionally not `Clone`; dropping it releases
    /// the claim and permits a later owner to resume the execution.
    pub fn claim_execution(&self, execution_id: ExecId) -> Result<ExecutionStore, StoreError> {
        let mut claims = self
            .inner
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
        self.run(move |db| db.load_execution_request(execution_id))
            .await
    }

    async fn bind_join_target(
        &self,
        execution_id: ExecId,
        target: NegotiationTarget,
    ) -> Result<AdmissionBindingOutcome, StoreError> {
        self.run(move |db| db.bind_join_target(execution_id, target))
            .await
    }

    /// List bounded admission roots in their durable creation order.
    pub async fn list_execution_requests(
        &self,
        limit: usize,
    ) -> Result<Vec<ExecutionRequest>, StoreError> {
        self.run(move |db| db.list_execution_requests(limit)).await
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
        self.run(move |db| db.list_recovery_candidates(cursor, limit))
            .await
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
            return Err(StoreError::PayloadTooLarge {
                required: wasm.len(),
                capacity: max,
            });
        }
        let hash = ProgramHash::of(&wasm);
        let outcome = self
            .run(move |db| db.register_program(hash, wasm, now_ms))
            .await?;
        Ok((hash, outcome))
    }

    /// Load one exact content-addressed Wasm program.
    pub async fn load_program(
        &self,
        hash: ProgramHash,
    ) -> Result<Option<StoredProgram>, StoreError> {
        self.run(move |db| db.load_program(hash)).await
    }

    /// List bounded content addresses for registry recovery.
    pub async fn list_programs(&self, limit: usize) -> Result<Vec<ProgramHash>, StoreError> {
        self.run(move |db| db.list_programs(limit)).await
    }

    /// Unregister a program from the active catalog while retaining its bytes
    /// for existing durable execution recovery.
    pub async fn remove_program(
        &self,
        hash: ProgramHash,
        now_ms: u64,
    ) -> Result<ProgramRemoveOutcome, StoreError> {
        self.run(move |db| db.remove_program(hash, now_ms)).await
    }

    /// Load a permanent activation record.
    pub async fn load_activation(
        &self,
        execution_id: ExecId,
    ) -> Result<Option<ActivationRecord>, StoreError> {
        self.run(move |db| db.load_activation(execution_id)).await
    }

    /// Load and validate one execution aggregate.
    pub async fn load_execution(
        &self,
        execution_id: ExecId,
    ) -> Result<Option<ExecutionState>, StoreError> {
        self.run(move |db| db.load_execution(execution_id)).await
    }

    /// List bounded execution aggregates for restart recovery.
    pub async fn list_executions(&self, limit: usize) -> Result<Vec<ExecutionState>, StoreError> {
        self.run(move |db| db.list_executions(limit)).await
    }

    /// Read a bounded public trace range from the durable execution.
    ///
    /// The store validates the complete trace before selecting the requested
    /// range. A malformed or gapped prefix therefore cannot become a
    /// plausible daemon event projection.
    pub async fn read_trace(
        &self,
        execution_id: ExecId,
        from: u64,
        to: u64,
    ) -> Result<Vec<arena0_protocol::TraceEntry>, StoreError> {
        self.run(move |db| db.read_trace(execution_id, from, to))
            .await
    }

    /// Read a bounded projection of durable local event records.
    ///
    /// Returned values contain only event/effect kinds, payload sizes, and
    /// durable coordinates; raw state and payloads never cross this capability
    /// boundary.
    pub async fn read_event_summaries(
        &self,
        execution_id: ExecId,
        from: Option<u64>,
        limit: usize,
    ) -> Result<EventInspectionPage, StoreError> {
        if limit > MAX_EVENT_INSPECTION_RECORDS {
            return Err(StoreError::InvalidConfiguration(
                "event inspection limit exceeds the fixed bound",
            ));
        }
        if limit == 0 {
            return Err(StoreError::InvalidConfiguration(
                "event inspection limit must be non-zero",
            ));
        }
        self.run(move |db| db.read_event_summaries(execution_id, from, limit))
            .await
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
        self.run(move |db| db.import_receipt(receipt, now_ms)).await
    }

    /// Load this Host's own publication for a session.
    pub async fn load_receipt(
        &self,
        session_id: SessionHash,
    ) -> Result<Option<StoredReceipt>, StoreError> {
        self.run(move |db| db.load_receipt(session_id)).await
    }

    /// Load one receipt by its content-addressed identity.
    pub async fn load_receipt_by_id(
        &self,
        receipt_id: ReceiptId,
    ) -> Result<Option<StoredReceipt>, StoreError> {
        self.run(move |db| db.load_receipt_by_id(receipt_id)).await
    }

    /// List receipts in deterministic key order.
    pub async fn list_receipts(&self, limit: usize) -> Result<Vec<StoredReceipt>, StoreError> {
        self.run(move |db| db.list_receipts(limit)).await
    }

    /// Load the optional durable Host user agent.
    pub async fn load_user_agent(&self) -> Result<Option<String>, StoreError> {
        self.run(move |db| db.load_user_agent()).await
    }

    /// Persist the Host user agent in the store metadata.
    pub async fn set_user_agent(&self, value: String) -> Result<(), StoreError> {
        validate_user_agent(&value)?;
        self.run(move |db| db.set_user_agent(value)).await
    }

    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Database) -> Result<T, StoreError> + Send + 'static,
    ) -> Result<T, StoreError> {
        let inner = Arc::clone(&self.inner);
        tokio::task::spawn_blocking(move || {
            let mut guard = inner.db.lock().map_err(|_| StoreError::Closed)?;
            let db = guard.as_mut().ok_or(StoreError::Closed)?;
            if db.is_poisoned() {
                return Err(StoreError::Closed);
            }
            f(db)
        })
        .await
        .map_err(|_| StoreError::Closed)?
    }
}

impl Drop for ExecutionStore {
    fn drop(&mut self) {
        let mut claims = self
            .handle
            .inner
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
            return Err(StoreError::PayloadTooLarge {
                required: params_len,
                capacity: arena0_protocol::MAX_PARAMS_LEN,
            });
        }
        let execution_id = self.execution_id;
        self.handle
            .run(move |db| {
                db.create_execution_request(
                    execution_id,
                    program_hash,
                    params.map(JsonBytes::into_bytes),
                    admission,
                    created_at_ms,
                )
            })
            .await
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
        let execution_id = self.execution_id;
        self.handle
            .run(move |db| db.record_execution_request_failure(execution_id, reason))
            .await
    }

    /// Load the per-execution local secret, creating it durably on first use.
    pub async fn load_or_create_execution_salt(
        &mut self,
        now_ms: u64,
    ) -> Result<ExecutionSalt, StoreError> {
        let execution_id = self.execution_id;
        self.handle
            .run(move |db| db.load_or_create_execution_salt(execution_id, now_ms))
            .await
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
        let execution_id = self.execution_id;
        self.handle
            .run(move |db| db.prepare_activation(execution_id, prepared, now_ms))
            .await
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
        let execution_id = self.execution_id;
        self.handle
            .run(move |db| db.commit_activation(execution_id, activation, now_ms))
            .await
    }

    /// Load this execution's permanent activation record.
    pub async fn load_activation(&self) -> Result<Option<ActivationRecord>, StoreError> {
        self.handle.load_activation(self.execution_id).await
    }

    /// Load and validate this execution's aggregate.
    pub async fn load_execution(&self) -> Result<Option<ExecutionState>, StoreError> {
        self.handle.load_execution(self.execution_id).await
    }

    /// Load one receipt by the content identity stored on this execution's
    /// published terminal status. The caller supplies the identity so it can
    /// check that the aggregate and immutable artifact remain bound together.
    pub async fn load_receipt_by_id(
        &self,
        receipt_id: ReceiptId,
    ) -> Result<Option<StoredReceipt>, StoreError> {
        self.handle.load_receipt_by_id(receipt_id).await
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
        let execution_id = self.execution_id;
        self.handle
            .run(move |db| {
                db.create_execution(
                    execution_id,
                    activation,
                    producer,
                    shared_state,
                    local_state,
                    now_ms,
                )
            })
            .await
    }

    /// Persist an actor-computed transition in one SQLite transaction.
    /// A moved version is corruption, not a retry signal. On any error the
    /// caller must reload committed state before making another transition:
    /// task cancellation or a database error can leave the outcome unknown.
    pub async fn persist(&mut self, record: TransitionRecord) -> Result<(), StoreError> {
        let execution_id = self.execution_id;
        self.handle
            .run(move |db| db.persist(execution_id, record))
            .await
    }

    /// Assemble portable evidence using the actor's committed state and durable
    /// trace rows. The state must belong to this execution at its stored version.
    pub async fn assemble_receipt(
        &self,
        state: &ExecutionState,
    ) -> Result<ReceiptArtifact, StoreError> {
        let execution_id = self.execution_id;
        let state = state.clone();
        self.handle
            .run(move |db| db.assemble_receipt(execution_id, &state))
            .await
    }

    /// List due active timers for this execution.
    pub async fn due_timers(
        &self,
        now_ms: u64,
        limit: usize,
    ) -> Result<Vec<ActiveTimer>, StoreError> {
        let execution_id = self.execution_id;
        self.handle
            .run(move |db| db.due_timers(execution_id, now_ms, limit))
            .await
    }
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
