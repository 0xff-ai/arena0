//! Summary rows: what list reads return, read from the scalar index columns
//! of `exec_requests`, `activation_records`, `executions` and `receipts`.
//!
//! The authoritative records stay the enveloped blobs. The columns read here
//! are written in the same transaction as those blobs, by the same store
//! method, from the same in-memory value, so a summary row never disagrees
//! with a committed blob. Building a summary row opens no envelope, decodes
//! no protocol aggregate and verifies no certificate or signature: list reads
//! trust what the store validated when it wrote the record (design §3.5).
//! Single-item reads (`load_execution`, `load_receipt_by_id`) still decode and
//! validate.

use arena0_program::ProgramHash;
use arena0_protocol::{
    CalloutId, EndPhase, ExecId, ExecLifecycle, NegotiationId, OfferHash, PeerId,
    PreparedActivation, ReceiptId, ReceiptKind, ReceiptProvenance, SessionHash, StateHash,
    TicketHash,
};

/// One execution request and whatever activation and execution aggregate
/// exist for it, as index columns. One row per `exec_requests` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecSummaryRow {
    pub execution_id: ExecId,
    pub program_hash: ProgramHash,
    /// From the `exec_requests.negotiation_id` column, written at request
    /// creation from the admission.
    pub negotiation_id: Option<NegotiationId>,
    /// `exec_requests.failure`.
    pub request_failure: Option<String>,
    pub created_at_ms: u64,
    pub activation: Option<ActivationIndex>,
    pub execution: Option<ExecutionIndex>,
}

/// `activation_records` scalars. `session_id` is the candidate hash while
/// prepared and the session identity once committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationIndex {
    pub committed: bool,
    pub session_id: SessionHash,
    pub updated_at_ms: u64,
    /// The display facts of the activation record, from the
    /// `activation_records.facts` column.
    pub facts: ActivationFacts,
}

/// What a list shows about an activation, written as one borsh column
/// (`activation_records.facts`) by `prepare_activation` and rewritten by
/// `commit_activation`, in the same transaction as the enveloped records,
/// from the activation values being stored. Reading it is a plain borsh decode
/// of this struct, not protocol decoding or validation.
#[derive(Debug, Clone, PartialEq, Eq, borsh::BorshSerialize, borsh::BorshDeserialize)]
pub struct ActivationFacts {
    pub negotiation_id: NegotiationId,
    pub offer_hash: OfferHash,
    pub creator: PeerId,
    pub target_size: u16,
    pub initial_state: StateHash,
    /// Canonical activation order, with each participant's ticket hash.
    pub participants: Vec<(PeerId, TicketHash)>,
    /// The offer params JSON bytes, as signed (program-owned; not parsed).
    pub params: Vec<u8>,
}

impl ActivationFacts {
    /// The display facts of `prepared`, as written to `activation_records.facts`.
    pub(crate) fn of(prepared: &PreparedActivation) -> Self {
        let offer = prepared.offer().data();
        Self {
            negotiation_id: offer.negotiation_id,
            offer_hash: OfferHash::of(offer),
            creator: offer.creator,
            target_size: offer.target_size,
            initial_state: offer.initial_state,
            participants: prepared
                .tickets()
                .iter()
                .map(|ticket| (ticket.data.signer, TicketHash::of(&ticket.data)))
                .collect(),
            params: offer.params.as_bytes().to_vec(),
        }
    }
}

/// `executions` scalars, written by `insert_execution` and `persist_state`
/// from the state being stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionIndex {
    pub session_id: SessionHash,
    pub lifecycle: ExecLifecycle,
    pub agreed_step: u64,
    /// `certified_at_ms` of the agreed step `agreed_step - 1`, i.e. when this
    /// Host stored its latest agreed step; `None` before the first step.
    pub last_step_at_ms: Option<u64>,
    pub end: EndPhase,
    /// Committed ensemble size (`binding.activation().tickets().len()`).
    pub participants: usize,
    /// Every committed participant in canonical order, the local Host
    /// included. Callers that want remote peers filter out their own id.
    pub participant_ids: Vec<PeerId>,
    pub callout: Option<CalloutIndex>,
    /// Terminal cause reason for aborted/failed aggregates, else `None`.
    pub terminal_reason: Option<String>,
    /// `state.terminal_outcome_json()` bytes for completed aggregates, else
    /// `None`. Program-owned JSON; the store does not parse it.
    pub outcome_json: Option<Vec<u8>>,
    /// Whether this Host produced a receipt for `session_id`
    /// (`receipt_productions` joined with `receipts`), by `EXISTS`, without
    /// loading the artifact.
    pub receipt_produced: bool,
    pub updated_at_ms: u64,
}

/// The open callout's identity and the local time it opened. `opened_at_ms`
/// is the `now_ms` of the transition that first stored this callout id; later
/// transitions that keep the same id keep the stored time (design §3.1, user
/// ruling: recency is identical before and after a reload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CalloutIndex {
    // `opened_at_ms` is a local observation like `agreed_steps.certified_at_ms`:
    // the column is its only owner. Nothing in the execution state records it.
    pub id: CalloutId,
    pub callout_index: u32,
    pub opened_at_ms: u64,
}

/// One receipt artifact as index columns. `program_hash` and `completed` are
/// written at insert, when the artifact is authenticated anyway.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptSummaryRow {
    pub receipt_id: ReceiptId,
    pub session_id: SessionHash,
    pub kind: ReceiptKind,
    pub program_hash: ProgramHash,
    pub completed: bool,
    pub provenance: ReceiptProvenance,
    pub stored_at_ms: u64,
}

/// One execution's agreed steps `from_step..` as parallel arrays, read from
/// `agreed_steps` index columns (design §3.7, ruling 3: compact step arrays).
/// `certified_at_ms[i]` and `post_state[i]` belong to step `from_step + i`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepTimesRow {
    pub exec_id: ExecId,
    pub from_step: u64,
    /// Local time this Host stored each step (`agreed_steps.certified_at_ms`).
    pub certified_at_ms: Vec<u64>,
    /// Each step's post-state hash (`agreed_steps.post_state`), written from
    /// the step's entry in the same transaction as its artifact.
    pub post_state: Vec<StateHash>,
}
