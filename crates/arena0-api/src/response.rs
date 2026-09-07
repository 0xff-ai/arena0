//! Responses on the daemon's local surface. The wire type is
//! [`Response`] (`Result<ResponseOk, ApiError>`), so a client matches success vs.
//! failure once and then on the typed payload.

use arena0_crypto::AgentPubKey;
use arena0_program::{JsonSchemaDocument, ParticipantCount, ProgramHash, ProgramSchema};
use arena0_protocol::{
    ExecId, ExecLifecycle, NegotiationId, OfferHash, PeerId, PendingId, ReceiptArtifact,
    SessionHash, StateHash, StopCause, TicketHash, TraceEntry, View,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One response frame.
pub type Response = Result<ResponseOk, ApiError>;

/// The successful payload, one variant per request shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub enum ResponseOk {
    /// A method with no payload succeeded (e.g. `daemon.stop`, `program.remove`).
    Ack,
    Id(IdInfo),
    IdList(Vec<IdInfo>),
    Program(Box<ProgramDetail>),
    ProgramList(Vec<ProgramSummary>),
    /// `exec.new` returns immediately; negotiation runs in the background. The state
    /// is `Negotiating` (or its later observable states); `queue_position` is `Some`
    /// when this creation waits behind another daemon-wide negotiation.
    ExecCreated {
        exec_id: ExecId,
        negotiation_id: NegotiationId,
        /// Absent until the N-of-N activation is durably stored.
        session_id: Option<SessionHash>,
        exec_state: ExecLifecycle,
        queue_position: Option<usize>,
    },
    ExecList(Vec<ExecStatus>),
    Status(ExecStatus),
    /// `exec.inspect`: an additive, bounded local diagnostic projection.
    Inspection(ExecutionInspection),
    /// `exec.await` reached the requested state. When it ended in a terminal
    /// state, `reason` carries the cached terminal reason (negotiation failure,
    /// abort), if any.
    Awaited {
        exec_id: ExecId,
        exec_state: ExecLifecycle,
        reason: Option<String>,
    },
    Next(NextEvent),
    /// `exec.query`: the guest-produced JSON response to a validated query.
    Query {
        result: Value,
    },
    /// `exec.view`: the program-authored view rendered for a terminal viewport.
    ExecView {
        step: u64,
        view: View,
    },
    Trace(Vec<TraceEntry>),
    Receipt(Box<ReceiptArtifact>),
    ReceiptList(Vec<ReceiptListEntry>),
    /// `receipt.verify`: the recovered evidence, not a bool. The result variant
    /// records whether structural light verification or Wasm replay ran, so a
    /// completed result cannot claim an unavailable JSON projection.
    Verified {
        receipt_id: arena0_protocol::ReceiptId,
        program_id: ProgramHash,
        session_id: SessionHash,
        ensemble: Vec<PeerId>,
        steps: u64,
        result: VerifiedResult,
    },
    DaemonInfo(DaemonInfo),
    /// The ack for `events.subscribe`; `EventFrame`s follow on the same connection.
    Subscribed,
}

/// Public metadata for one Host. The local id locates its namespace; the peer
/// identity belongs to its retained keys. User agent is caller-reported metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostInfo {
    pub id: String,
    pub peer_id: PeerId,
    pub user_agent: Option<String>,
}

/// The `daemon.info` response for one Host supervised by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub host: HostInfo,
    pub transport_key: AgentPubKey,
    pub version: String,
    pub abi_version: u32,
    pub uptime_secs: u64,
    pub socket: String,
    pub programs: usize,
    pub execs_active: usize,
}

/// A structured error a client can match on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct ApiError {
    pub code: ApiErrorCode,
    pub message: String,
}

impl ApiError {
    pub fn new(code: ApiErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

/// Error categories. The daemon maps internal errors onto these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiErrorCode {
    /// The referenced identity, program, execution, or receipt was not found.
    NotFound,
    /// The request was malformed or referenced an invalid state.
    BadRequest,
    /// A program handle resolved to more than one program.
    Ambiguous,
    /// A JSON value failed validation against the program's public schema.
    Schema,
    /// Ticket negotiation or activation failed.
    Negotiation,
    /// The execution failed or was terminated.
    Execution,
    /// Verification rejected the receipt.
    Verification,
    /// The daemon could not access its keystore or persistence.
    Storage,
    /// A blocking operation exceeded its deadline.
    Timeout,
    /// An unexpected internal error.
    Internal,
}

/// Public material for one keystore identity. Seeds never appear here.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IdInfo {
    pub peer_id: PeerId,
    pub label: Option<String>,
    /// The persistent Ed25519 identity key used by the Host.
    /// `peer_id` is derived from this key. The execution BLS key is minted per exec,
    /// so it is not identity material and does not appear here.
    pub transport_key: AgentPubKey,
    /// Whether this is the daemon's active identity.
    pub active: bool,
}

/// A registry listing entry (no wasm bytes, no schema).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProgramSummary {
    pub program_hash: ProgramHash,
    pub name: String,
    pub display_name: String,
    pub version: String,
    pub description: String,
    pub participants: ParticipantCount,
}

/// `program.get`: a summary plus the full schema (callouts, messages, params, ...).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProgramDetail {
    pub summary: ProgramSummary,
    pub schema: ProgramSchema,
}

/// A daemon-owned public execution snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecStatus {
    pub exec_id: ExecId,
    pub negotiation_id: Option<NegotiationId>,
    pub program_id: ProgramHash,
    pub state: ExecStatusState,
}

/// The facts valid at each public execution lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "exec_state", deny_unknown_fields)]
pub enum ExecStatusState {
    Negotiating { queue_position: Option<usize> },
    Activating { session_id: Option<SessionHash> },
    Active { session: SessionStatus },
    Completed { session: SessionStatus },
    Aborted { session: SessionStatus },
    Failed { session: Option<SessionProgress> },
}

/// Session facts retained after a session starts.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SessionStatus {
    pub session_id: SessionHash,
    pub step: u64,
    /// The committed remote participants. This excludes the local Host and is
    /// not a transport-liveness view.
    pub peers: Vec<PeerId>,
    /// The committed ensemble size, not the live transport stream count.
    pub participants: usize,
    pub pending_callout: Option<PendingCalloutStatus>,
    /// Whether this Host's locally produced artifact is durably available.
    pub receipt_available: bool,
}

/// A bounded, host-local projection of durable execution facts for inspection
/// UIs. It contains no signatures, keys, parameters, outcomes, callout
/// contexts, or private payload bytes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ExecutionInspection {
    /// The ordinary lifecycle projection for this execution.
    pub status: ExecStatus,
    /// The durable activation evidence, when preparation has started.
    pub activation: Option<ActivationInspection>,
    /// The returned private-record window's first sequence.
    pub private_from: u64,
    /// The bounded private-record projection.
    pub private: Vec<PrivateCommitSummary>,
    /// Total private records currently durable for this execution.
    pub private_total: u64,
    /// Next sequence available after this page, if more records exist.
    pub private_next: Option<u64>,
}

/// Durable activation facts safe to display in a local diagnostic view.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActivationInspection {
    /// Whether the permanent record is prepared or committed.
    pub state: ActivationInspectionState,
    /// Negotiation identity fixed by the offer.
    pub negotiation_id: NegotiationId,
    /// Session identity exists only after the activation certificate commits.
    pub session_id: Option<SessionHash>,
    /// Commitment to the frozen offer terms.
    pub offer_hash: OfferHash,
    /// Creator identity fixed by the offer.
    pub creator: PeerId,
    /// Target participant count fixed by the offer.
    pub target_size: u16,
    /// Initial public state commitment fixed by the offer.
    pub initial_state: StateHash,
    /// All selected participants in canonical activation order.
    pub participants: Vec<ActivationParticipant>,
}

/// State of a durable activation record.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ActivationInspectionState {
    Prepared,
    Committed,
}

/// One participant and its ticket commitment. Ticket bodies and signatures
/// intentionally remain host-owned and are not exposed by this projection.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ActivationParticipant {
    pub peer_id: PeerId,
    pub ticket_hash: TicketHash,
}

/// Kind of local event that caused one private handler run.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrivateEventKind {
    InputReceived,
    TimerFired,
    TypedTimerFired,
    Signed,
    React,
}

/// Kind and bounded payload size of one private effect. The payload itself is
/// deliberately never returned.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PrivateEffectSummary {
    pub kind: PrivateEffectKind,
    pub payload_bytes: Option<u64>,
}

/// Kind of effect emitted by one private handler run.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrivateEffectKind {
    Broadcast,
    Callout,
    SetTimer,
    Sign,
    RetryInput,
}

/// A bounded summary of one durable private handler commit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PrivateCommitSummary {
    /// Gapless local private-record sequence.
    pub sequence: u64,
    /// Public cursor observed by the local run.
    pub public_position: u64,
    pub event: PrivateEventKind,
    /// Size of the input payload, when the event has one. The payload is not
    /// exposed.
    pub input_payload_bytes: Option<u64>,
    pub effects: Vec<PrivateEffectSummary>,
    pub fuel_used: u64,
}

/// How far a failed execution got before it stopped.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "session_state", deny_unknown_fields)]
pub enum SessionProgress {
    Activated { session_id: SessionHash },
    Started { session: SessionStatus },
}

/// The public identity of an agent-visible pending callout.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PendingCalloutStatus {
    pub pending_id: PendingId,
    pub callout_index: u32,
    pub expected_type: Option<String>,
}

impl ExecStatus {
    #[must_use]
    pub fn lifecycle(&self) -> ExecLifecycle {
        self.state.lifecycle()
    }

    #[must_use]
    pub fn session(&self) -> Option<&SessionStatus> {
        self.state.session()
    }

    #[must_use]
    pub fn session_id(&self) -> Option<SessionHash> {
        self.state.session_id()
    }

    #[must_use]
    pub fn step(&self) -> Option<u64> {
        self.session().map(|session| session.step)
    }

    #[must_use]
    pub fn peers(&self) -> Option<&[PeerId]> {
        self.session().map(|session| session.peers.as_slice())
    }

    #[must_use]
    pub fn participants(&self) -> Option<usize> {
        self.session().map(|session| session.participants)
    }

    #[must_use]
    pub fn queue_position(&self) -> Option<usize> {
        match &self.state {
            ExecStatusState::Negotiating { queue_position } => *queue_position,
            _ => None,
        }
    }

    #[must_use]
    pub fn pending_callout(&self) -> Option<&PendingCalloutStatus> {
        self.session()
            .and_then(|session| session.pending_callout.as_ref())
    }
}

impl ExecStatusState {
    #[must_use]
    pub fn lifecycle(&self) -> ExecLifecycle {
        match self {
            Self::Negotiating { .. } => ExecLifecycle::Negotiating,
            Self::Activating { .. } => ExecLifecycle::Activating,
            Self::Active { .. } => ExecLifecycle::Active,
            Self::Completed { .. } => ExecLifecycle::Completed,
            Self::Aborted { .. } => ExecLifecycle::Aborted,
            Self::Failed { .. } => ExecLifecycle::Failed,
        }
    }

    fn session(&self) -> Option<&SessionStatus> {
        match self {
            Self::Active { session } | Self::Completed { session } | Self::Aborted { session } => {
                Some(session)
            }
            Self::Failed {
                session: Some(SessionProgress::Started { session }),
            } => Some(session),
            _ => None,
        }
    }

    fn session_id(&self) -> Option<SessionHash> {
        match self {
            Self::Activating { session_id } => *session_id,
            Self::Active { session }
            | Self::Completed { session }
            | Self::Aborted { session }
            | Self::Failed {
                session: Some(SessionProgress::Started { session }),
            } => Some(session.session_id),
            Self::Failed {
                session: Some(SessionProgress::Activated { session_id }),
            } => Some(*session_id),
            Self::Negotiating { .. } | Self::Failed { session: None } => None,
        }
    }
}

/// The blocking return of `exec.next`. Signing continuations stay inside the
/// execution actor, which owns the custodied key.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum NextEvent {
    /// The program is asking the agent to decide. `name`/`prompt` come from the
    /// program schema, `schema` is the answer type inline (so a cold agent needs no
    /// `get_program`), and `context` is guest-produced JSON.
    Callout {
        pending_id: PendingId,
        callout_index: u32,
        name: String,
        prompt: String,
        schema: JsonSchemaDocument,
        context: Value,
    },
    /// The session completed. The program can omit a terminal outcome.
    Completed {
        session_id: SessionHash,
        outcome: Option<Value>,
    },
    /// The execution failed or aborted.
    Failed { reason: String },
}

/// The local facts known about a stored receipt artifact.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptProvenance {
    /// The daemon produced the artifact.
    Produced,
    /// The daemon imported the artifact from another Host.
    Imported,
    /// The daemon imported the artifact and later produced the same artifact.
    Both,
}

/// A `receipt.list` entry: the content address, its session and producer, program,
/// completion, and the complete local provenance projection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReceiptListEntry {
    pub receipt_id: String,
    pub session_id: SessionHash,
    pub kind: arena0_protocol::ReceiptKind,
    pub program_id: ProgramHash,
    pub completed: bool,
    pub provenance: ReceiptProvenance,
}

/// The result of one verification tier.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub enum VerifiedResult {
    /// Structural and cryptographic verification without Wasm.
    Light {
        /// Terminal evidence available without executing the guest.
        terminal: LightVerifiedTerminal,
    },
    /// Structural verification followed by deterministic Wasm replay.
    Full {
        /// Terminal evidence including the replayed JSON projection when the
        /// receipt completed.
        terminal: FullVerifiedTerminal,
    },
}

/// Terminal evidence returned by light verification.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub enum LightVerifiedTerminal {
    /// The receipt records a signed completion. Light verification leaves the
    /// JSON projection unavailable because it does not load the guest.
    Completed {
        /// Opaque stock-Borsh outcome bytes committed by the receipt.
        outcome_borsh: Vec<u8>,
    },
    /// The receipt records an authenticated unilateral stop or a shared
    /// N-of-N stop. The protocol evidence remains intact for callers.
    Stopped { cause: StopCause },
}

/// Terminal evidence returned by full verification.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub enum FullVerifiedTerminal {
    /// The receipt records a signed completion and replay produced the JSON
    /// projection from the guest's concrete outcome DTO.
    Completed {
        /// Opaque stock-Borsh outcome bytes committed by the receipt.
        outcome_borsh: Vec<u8>,
        /// Guest-produced agent-facing JSON. Full completion always has this
        /// value; stopped proofs have no outcome field at all.
        outcome_json: Value,
    },
    /// The receipt records an authenticated unilateral stop or a shared
    /// N-of-N stop. The protocol evidence remains intact for callers.
    Stopped { cause: StopCause },
}
