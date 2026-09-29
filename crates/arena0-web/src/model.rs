//! Rows the gateway replicates to the browser.
//!
//! Every row is a display projection of facts the daemon owns; the gateway
//! never invents a fact and the browser never writes one. Ids and hashes are
//! lowercase hex strings. Times are Unix milliseconds on this machine's clock.
//! A row's `key` is unique within its collection and stable for its lifetime.

use serde::Serialize;
use serde_json::Value;
use ts_rs::TS;

/// One local Host. Key: `id`.
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
pub struct HostRow {
    /// Local namespace such as `host-01`.
    pub id: String,
    pub peer_id: String,
    pub user_agent: Option<String>,
    pub transport_key: String,
    pub programs: u32,
    pub execs_active: u32,
    /// False after `host.stopped` until the next `host.started`.
    pub online: bool,
    /// `stream.lagged` frames seen for this Host since the gateway started.
    /// Each one made the gateway reload the Host from durable reads.
    pub gaps: u32,
    pub last_gap_ms: Option<u64>,
}

/// One Host's execution. Key: `{host}/{exec_id}`.
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
pub struct ExecutionRow {
    pub key: String,
    pub host: String,
    pub exec_id: String,
    /// Program content hash.
    pub program: String,
    pub lifecycle: Lifecycle,
    pub negotiation_id: Option<String>,
    pub session_id: Option<String>,
    pub queue_position: Option<u32>,
    /// Index of the latest agreed step (steps are numbered from 0, like
    /// `StepRow.step`); `None` until step 0 is certified. The daemon's
    /// `SessionStatus.step` counts agreed steps, so this is that count minus
    /// one.
    pub latest_step: Option<u64>,
    /// Committed ensemble size.
    pub participants: Option<u32>,
    /// Committed remote participants; excludes this Host.
    pub peers: Vec<String>,
    pub pending: Option<PendingRef>,
    pub receipt_available: bool,
    pub end: EndRow,
    /// Participant expected to author the next step, when the program has one.
    pub writer: Option<String>,
    /// Current program phase name, when the program declares phases.
    pub phase: Option<String>,
    pub activation: Option<ActivationRow>,
    /// Durable local creation time.
    pub created_ms: u64,
    /// Durable local time of the last lifecycle change.
    pub updated_ms: u64,
    pub terminal: Option<TerminalRow>,
    /// Negotiation progress this gateway observed live. Empty for negotiations
    /// that ran while no gateway was watching; never reconstructed.
    pub negotiation: Vec<NegotiationMark>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum Lifecycle {
    Negotiating,
    Activating,
    Active,
    Completed,
    Aborted,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct PendingRef {
    pub pending_id: String,
    pub callout_index: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct EndRow {
    pub phase: EndPhase,
    /// Peers that have not confirmed the end.
    pub unconfirmed: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum EndPhase {
    Open,
    Ending,
    Ended,
}

/// Durable activation facts: the offer terms every participant agreed to.
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
pub struct ActivationRow {
    pub state: ActivationState,
    pub offer_hash: String,
    pub creator: String,
    pub target_size: u16,
    pub initial_state: String,
    /// All selected participants in participant order (sorted peer ids, as
    /// the committed ensemble orders them). Index `i` is participant `P{i}`
    /// everywhere in the UI, matching step signers and view cells.
    pub participants: Vec<ActivationParticipantRow>,
    /// Offer params as JSON.
    pub params: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ActivationState {
    Prepared,
    Committed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct ActivationParticipantRow {
    pub peer_id: String,
    pub ticket_hash: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, TS)]
pub struct TerminalRow {
    pub kind: TerminalKind,
    /// Terminal reason for aborts and failures.
    pub reason: Option<String>,
    /// JSON outcome of a completed session, when this gateway observed it.
    pub outcome: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum TerminalKind {
    Completed,
    Aborted,
    Failed,
}

/// One observed `exec.negotiation.*` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct NegotiationMark {
    pub at_ms: u64,
    pub kind: NegotiationMarkKind,
    /// One line with the event's counts, e.g. `ticket 2/4` or `retried 3 at prepared`.
    pub detail: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum NegotiationMarkKind {
    Started,
    OfferAccepted,
    TicketAccepted,
    Peers,
    Prepared,
    Resumed,
    Committed,
    Retried,
    Rejoined,
    TimedOut,
}

/// One agreed step as one local Host holds it. Key: `{host}/{exec_id}/{step}`.
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
pub struct StepRow {
    pub key: String,
    pub host: String,
    pub exec_id: String,
    pub session_id: String,
    pub step: u64,
    /// Local time this Host durably stored the certified step.
    pub certified_ms: u64,
    pub event: StepEventRow,
    pub pre_state: String,
    pub post_state: String,
    /// Canonical participant indexes whose signatures are in the aggregate.
    pub signers: Vec<u16>,
    /// Committed ensemble size.
    pub participants: u16,
    pub terminal: Option<StepTerminalRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepEventRow {
    /// Step 0.
    SessionStarted { ensemble: Vec<String> },
    Message {
        from: String,
        bytes: u32,
        /// The payload decoded with the program's Borsh message schema.
        decoded: Option<Value>,
        /// Why decoding failed, when it did.
        decode_error: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StepTerminalRow {
    End { outcome_bytes: u32 },
    Abort { reason: String },
    Fail { reason: String },
}

/// One open callout. Key: `{host}/{exec_id}/{pending_id}`. Deleted when it is
/// answered or its execution leaves `active`.
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
pub struct CalloutRow {
    pub key: String,
    pub host: String,
    pub exec_id: String,
    pub session_id: Option<String>,
    pub pending_id: String,
    pub callout_index: u32,
    pub name: String,
    pub prompt: String,
    /// JSON Schema (Draft 2020-12) of the answer.
    pub schema: Value,
    /// Guest-produced context.
    pub context: Value,
    /// When the callout became pending: the event time when observed live,
    /// otherwise the certified time of the step it follows.
    pub opened_ms: u64,
}

/// One program, merged across the Hosts that hold it. Key: `hash`.
#[derive(Debug, Clone, PartialEq, Serialize, TS)]
pub struct ProgramRow {
    pub hash: String,
    pub name: String,
    pub display_name: String,
    pub version: String,
    pub description: String,
    pub participants: ParticipantRange,
    /// Hosts whose catalog holds the program.
    pub hosts: Vec<String>,
    pub schema: ProgramSchemaRow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
pub struct ParticipantRange {
    pub min: u16,
    pub max: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, TS)]
pub struct ProgramSchemaRow {
    pub params: Value,
    pub state: Value,
    pub outcome: Value,
    pub callouts: Vec<CalloutSchemaRow>,
    pub queries: Vec<QuerySchemaRow>,
    pub phases: Vec<PhaseRow>,
}

#[derive(Debug, Clone, PartialEq, Serialize, TS)]
pub struct CalloutSchemaRow {
    pub name: String,
    pub prompt: String,
    pub input: Value,
    pub output: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, TS)]
pub struct QuerySchemaRow {
    pub name: String,
    pub label: String,
    pub request: Value,
    pub response: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct PhaseRow {
    pub name: String,
    pub description: String,
    pub is_default: bool,
    pub is_terminal: bool,
}

/// One receipt or stop report held by one Host. Key: `{host}/{receipt_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct ReceiptRow {
    pub key: String,
    pub host: String,
    pub receipt_id: String,
    pub session_id: String,
    pub kind: ReceiptKindRow,
    pub program: String,
    pub completed: bool,
    pub provenance: ProvenanceRow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptKindRow {
    Receipt,
    StopReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceRow {
    Produced,
    Imported,
    Both,
}

/// An offer seen on a program topic. Key: `negotiation_id`. Offers are
/// live-only: the row is deleted once a local execution joins the
/// negotiation or 10 minutes after the offer was last seen.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct OfferRow {
    pub negotiation_id: String,
    pub program: String,
    pub creator: String,
    pub offer_seq: u64,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    /// Local Hosts that saw the offer.
    pub seen_by: Vec<String>,
}

/// One line of daemon activity: a Host event or an MCP tool call. Key:
/// `{source}/{boot_id}/{seq}`. The gateway keeps the latest 1000 rows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct ActivityRow {
    pub key: String,
    pub at_ms: u64,
    /// A Host id, or `mcp` for adapter tool calls.
    pub source: String,
    /// Wire tag such as `exec.session.step` or `tool.finished`.
    pub kind: String,
    pub exec_id: Option<String>,
    pub session_id: Option<String>,
    /// One line without payloads, answers, params, or outcomes.
    pub text: String,
    pub level: Level,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Info,
    Warn,
    Error,
}

/// One stored blob on one Host. Key: `{host}/{hash}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, TS)]
pub struct BlobRow {
    pub key: String,
    pub host: String,
    pub hash: String,
    pub length: u64,
    /// The daemon-local file the blob is linked to.
    pub path: String,
}
