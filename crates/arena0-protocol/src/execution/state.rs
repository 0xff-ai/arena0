use arena0_program::{CalloutRequest, LocalStateBytes, SharedStateBytes};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::de::Error as _;
use serde::{Deserialize, Serialize};
#[cfg(feature = "performance-tracing")]
use std::time::Instant;

use crate::exec::ExecLifecycle;
use crate::negotiation::Activation;
use crate::trace::{AggregateAttestation, StepCommitment, TRACE_FORMAT_VERSION, TraceEntry};
use crate::{
    Effect, Event, ExecFrame, ExecId, MessageId, OpenCallout, PeerId, PendingId, StateHash,
    pending_id,
};

use super::{
    ExecutionBinding, ExecutionStatus, ExecutionVersion, MAX_EFFECTS, MAX_EXECUTION_STATE_BYTES,
    MAX_PROOF_SIGNATURES, ParticipantStepSignature, ProtocolError, ReceiptArtifact, ReceiptId,
    StepCursor, TerminalOutcome, ensure_encoded, validate_effects, validate_proposal,
    validate_receipt_body,
};

/// A shared step waiting for N-of-N signatures.
///
/// The proposal is the only durable place where a dispatch result can be
/// staged before agreement. It keeps both state memories and the complete
/// post-agreement effect list because agreement commits the dispatch
/// atomically: a failed or incomplete agreement must not expose either memory
/// or any deferred effect. An establishing broadcast from a non-agreed event
/// is consumed into its normalized trace entry and precommit outbox instead
/// of being retained here; a broadcast emitted by an already-agreed event is
/// retained as deferred next-step work. `event_position` identifies the event
/// that produced the result; it is independent of the agreed trace step.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SharedProposal {
    pub(crate) commitment: StepCommitment,
    pub(crate) entry: TraceEntry,
    pub(crate) shared_state: SharedStateBytes,
    pub(crate) local_state: LocalStateBytes,
    /// Effects paired with their original dispatch ordinals. Establishing
    /// broadcasts may be omitted before staging; every retained effect keeps
    /// its original ordinal for durable outbox identity.
    pub(crate) effects: Vec<(u32, Effect)>,
    pub(crate) event_position: u64,
    pub(crate) status: ExecutionStatus,
    /// The open callout this proposal installs when it is certified.
    pub(crate) callout: Option<OpenCallout>,
    pub(crate) signatures: Vec<ParticipantStepSignature>,
}

impl SharedProposal {
    /// Construct a staged dispatch result.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        commitment: StepCommitment,
        entry: TraceEntry,
        shared_state: SharedStateBytes,
        local_state: LocalStateBytes,
        effects: Vec<(u32, Effect)>,
        event_position: u64,
        status: ExecutionStatus,
        callout: Option<OpenCallout>,
        signatures: Vec<ParticipantStepSignature>,
    ) -> Result<Self, ProtocolError> {
        if effects.len() > MAX_EFFECTS {
            return Err(ProtocolError::CollectionTooLarge {
                kind: "effects",
                actual: effects.len(),
                max: MAX_EFFECTS,
            });
        }
        validate_effects(&effects)?;
        let proposal = Self {
            commitment,
            entry,
            shared_state,
            local_state,
            effects,
            event_position,
            status,
            callout,
            signatures,
        };
        Ok(proposal)
    }

    /// Borrow the proposed trace entry.
    #[must_use]
    pub const fn entry(&self) -> &TraceEntry {
        &self.entry
    }

    /// Borrow the proposed shared state payload.
    #[must_use]
    pub const fn shared_state(&self) -> &SharedStateBytes {
        &self.shared_state
    }

    /// Borrow the proposed local state payload.
    #[must_use]
    pub const fn local_state(&self) -> &LocalStateBytes {
        &self.local_state
    }

    /// Borrow effects emitted by the dispatch.
    #[must_use]
    pub fn effects(&self) -> &[(u32, Effect)] {
        &self.effects
    }

    /// Return the event position that produced this result.
    #[must_use]
    pub const fn event_position(&self) -> u64 {
        self.event_position
    }

    /// Borrow the status to install when this proposal commits.
    #[must_use]
    pub const fn status(&self) -> &ExecutionStatus {
        &self.status
    }

    /// Borrow the open callout to install when this proposal commits.
    #[must_use]
    pub const fn callout(&self) -> Option<&OpenCallout> {
        self.callout.as_ref()
    }

    /// Borrow immutable collected participant signatures.
    #[must_use]
    pub fn signatures(&self) -> &[ParticipantStepSignature] {
        &self.signatures
    }

    /// Return the number of collected signatures.
    #[must_use]
    pub fn signature_count(&self) -> usize {
        self.signatures.len()
    }

    /// Borrow the trace-owned commitment being signed.
    #[must_use]
    pub const fn commitment(&self) -> &StepCommitment {
        &self.commitment
    }

    /// Add one participant's checked signature to this proposal.
    ///
    /// The proposal owns the signature list, but the activation binding owns
    /// participant membership and execution keys. Both are required before a
    /// signature is retained. The caller is responsible for persisting the
    /// enclosing [`ExecutionState`] so its version advances exactly once.
    pub fn add_signature(
        &mut self,
        binding: &ExecutionBinding,
        signature: ParticipantStepSignature,
    ) -> Result<(), ProtocolError> {
        let participant = signature.participant();
        if let Some(existing) = self
            .signatures
            .iter()
            .find(|existing| existing.participant() == participant)
        {
            if existing.signature() == signature.signature() {
                return Err(ProtocolError::DuplicateStepSignature { participant });
            }
            return Err(ProtocolError::ConflictingStepSignature { participant });
        }

        let key = binding.participant_key(&participant)?;
        if signature.signature().step != self.commitment.step {
            return Err(ProtocolError::InvalidStepSignature {
                participant,
                step: self.commitment.step,
            });
        }
        let valid = key
            .verify(&self.commitment.signing_bytes(), &signature.signature().sig)
            .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;
        if !valid {
            return Err(ProtocolError::InvalidStepSignature {
                participant,
                step: self.commitment.step,
            });
        }
        if self.signatures.len() >= MAX_PROOF_SIGNATURES {
            return Err(ProtocolError::CollectionTooLarge {
                kind: "step signatures",
                actual: self.signatures.len() + 1,
                max: MAX_PROOF_SIGNATURES,
            });
        }

        self.signatures.push(signature);
        self.signatures
            .sort_by_key(ParticipantStepSignature::participant);
        Ok(())
    }
}

/// An activation-bound N-of-N certificate for one shared trace step.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct StepCertificate {
    pub(crate) commitment: StepCommitment,
    pub(crate) agreement: AggregateAttestation,
}

impl StepCertificate {
    /// Borrow the signed commitment.
    #[must_use]
    pub const fn commitment(&self) -> &StepCommitment {
        &self.commitment
    }

    /// Borrow the N-of-N aggregate agreement.
    #[must_use]
    pub const fn agreement(&self) -> &AggregateAttestation {
        &self.agreement
    }
}

/// The one durable execution aggregate.
///
/// `event_position` is the next monotonic event coordinate. `agreed_step` is
/// separately the next portable trace position. The shared hash and chain
/// link always describe the last agreed step; local state and staged results
/// are intentionally outside that commitment.
#[derive(Serialize, Debug, Clone, PartialEq)]
pub struct ExecutionState {
    pub(crate) execution_id: ExecId,
    pub(crate) binding: ExecutionBinding,
    pub(crate) producer: PeerId,
    pub(crate) status: ExecutionStatus,
    pub(crate) version: ExecutionVersion,
    pub(crate) event_position: u64,
    pub(crate) agreed_step: u64,
    /// Sum of canonical encodings of certified entries, including agreements.
    pub(crate) trace_bytes: u64,
    /// Derived once from activation when constructing or decoding the aggregate.
    #[serde(skip)]
    receipt_overhead: u64,
    pub(crate) agreed_state: StateHash,
    pub(crate) agreed_link: [u8; 32],
    pub(crate) last_reacted_step: Option<u64>,
    pub(crate) shared_state: SharedStateBytes,
    pub(crate) local_state: LocalStateBytes,
    pub(crate) proposal: Option<SharedProposal>,
    pub(crate) callout: Option<OpenCallout>,
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
struct ExecutionStateBody {
    execution_id: ExecId,
    binding: ExecutionBinding,
    producer: PeerId,
    status: ExecutionStatus,
    version: ExecutionVersion,
    event_position: u64,
    agreed_step: u64,
    trace_bytes: u64,
    agreed_state: StateHash,
    agreed_link: [u8; 32],
    last_reacted_step: Option<u64>,
    shared_state: SharedStateBytes,
    local_state: LocalStateBytes,
    proposal: Option<SharedProposal>,
    callout: Option<OpenCallout>,
}

impl BorshSerialize for ExecutionState {
    fn serialize<W: borsh::io::Write>(&self, writer: &mut W) -> borsh::io::Result<()> {
        BorshSerialize::serialize(&self.body(), writer)
    }
}

impl BorshDeserialize for ExecutionState {
    fn deserialize_reader<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<Self> {
        let body = ExecutionStateBody::deserialize_reader(reader)?;
        Self::from_body(body).map_err(|error| {
            borsh::io::Error::new(borsh::io::ErrorKind::InvalidData, error.to_string())
        })
    }
}

impl<'de> Deserialize<'de> for ExecutionState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let body = <ExecutionStateBody as Deserialize>::deserialize(deserializer)?;
        Self::from_body(body).map_err(D::Error::custom)
    }
}

impl ExecutionState {
    fn body(&self) -> ExecutionStateBody {
        ExecutionStateBody {
            execution_id: self.execution_id,
            binding: self.binding.clone(),
            producer: self.producer,
            status: self.status.clone(),
            version: self.version,
            event_position: self.event_position,
            agreed_step: self.agreed_step,
            trace_bytes: self.trace_bytes,
            agreed_state: self.agreed_state,
            agreed_link: self.agreed_link,
            last_reacted_step: self.last_reacted_step,
            shared_state: self.shared_state.clone(),
            local_state: self.local_state.clone(),
            proposal: self.proposal.clone(),
            callout: self.callout.clone(),
        }
    }

    fn from_body(body: ExecutionStateBody) -> Result<Self, ProtocolError> {
        let receipt_overhead = ReceiptArtifact::reserved_overhead(&body.binding)?;
        let state = Self {
            execution_id: body.execution_id,
            binding: body.binding,
            producer: body.producer,
            status: body.status,
            version: body.version,
            event_position: body.event_position,
            agreed_step: body.agreed_step,
            trace_bytes: body.trace_bytes,
            receipt_overhead,
            agreed_state: body.agreed_state,
            agreed_link: body.agreed_link,
            last_reacted_step: body.last_reacted_step,
            shared_state: body.shared_state,
            local_state: body.local_state,
            proposal: body.proposal,
            callout: body.callout,
        };
        state.validate_recovered()?;
        Ok(state)
    }

    fn next_version(&self) -> Result<ExecutionVersion, ProtocolError> {
        self.version.next().ok_or(ProtocolError::VersionExhausted)
    }

    fn bump_version(&mut self) -> Result<(), ProtocolError> {
        self.version = self.next_version()?;
        Ok(())
    }

    /// Construct the initial aggregate for a validated activation.
    pub fn new(
        execution_id: ExecId,
        activation: Activation,
        producer: PeerId,
        shared_state: SharedStateBytes,
        local_state: LocalStateBytes,
    ) -> Result<Self, ProtocolError> {
        let binding = ExecutionBinding::new(activation)?;
        let receipt_overhead = ReceiptArtifact::reserved_overhead(&binding)?;
        if receipt_overhead > super::MAX_RECEIPT_BYTES as u64 {
            return Err(ProtocolError::ReceiptBudgetExhausted { step: 0 });
        }
        if !binding
            .activation
            .tickets()
            .iter()
            .any(|ticket| ticket.data.signer == producer)
        {
            return Err(ProtocolError::UnknownParticipant {
                participant: producer,
            });
        }
        if StateHash::of_shared(&shared_state) != binding.activation.offer().data().initial_state {
            return Err(ProtocolError::StateHashMismatch);
        }
        let cursor = StepCursor::genesis(&shared_state);
        Ok(Self {
            execution_id,
            binding,
            producer,
            status: ExecutionStatus::Activating,
            version: ExecutionVersion::ZERO,
            event_position: 0,
            agreed_step: cursor.next_step(),
            trace_bytes: 0,
            receipt_overhead,
            agreed_state: cursor.state_hash(),
            agreed_link: cursor.chain_hash(),
            last_reacted_step: None,
            shared_state,
            local_state,
            proposal: None,
            callout: None,
        })
    }

    /// Return the local execution identity.
    #[must_use]
    pub const fn execution_id(&self) -> ExecId {
        self.execution_id
    }

    /// Borrow the immutable activation binding.
    #[must_use]
    pub const fn binding(&self) -> &ExecutionBinding {
        &self.binding
    }

    /// Return the participant owning this execution.
    #[must_use]
    pub const fn producer(&self) -> PeerId {
        self.producer
    }

    /// Return the public lifecycle projection.
    ///
    /// An active execution with an open callout projects as `Waiting`; that
    /// fact is derived here rather than persisted in [`ExecutionStatus`].
    #[must_use]
    pub const fn lifecycle(&self) -> ExecLifecycle {
        if matches!(self.status, ExecutionStatus::Active) && self.callout.is_some() {
            return ExecLifecycle::Waiting;
        }
        self.status.lifecycle()
    }

    /// Borrow the durable execution status.
    #[must_use]
    pub const fn status(&self) -> &ExecutionStatus {
        &self.status
    }

    /// Borrow the committed open callout, if any.
    #[must_use]
    pub const fn callout(&self) -> Option<&OpenCallout> {
        self.callout.as_ref()
    }

    /// Return the receipt identity retained by a published terminal status.
    #[must_use]
    pub const fn published_receipt_id(&self) -> Option<ReceiptId> {
        match &self.status {
            ExecutionStatus::Completed { receipt_id, .. } => Some(*receipt_id),
            ExecutionStatus::StoppedPublished { receipt_id, .. } => Some(*receipt_id),
            ExecutionStatus::Activating
            | ExecutionStatus::Active
            | ExecutionStatus::Ended { .. }
            | ExecutionStatus::Stopped { .. } => None,
        }
    }

    /// Return the next event position.
    #[must_use]
    pub const fn event_position(&self) -> u64 {
        self.event_position
    }

    /// Return the next agreed trace step.
    #[must_use]
    pub const fn agreed_step(&self) -> u64 {
        self.agreed_step
    }

    /// Canonical encoded size of the certified public trace, excluding framing.
    #[must_use]
    pub const fn trace_bytes(&self) -> u64 {
        self.trace_bytes
    }

    /// Return the prospective trace size with an N-of-N agreement on this
    /// entry. Signature bytes have fixed width; the full signer bitmap must
    /// be counted even while the proposal's agreement is still empty.
    fn check_receipt_budget(&self, entry: &TraceEntry) -> Result<u64, ProtocolError> {
        self.check_receipt_entries(&[entry])
    }

    /// Reserve every entry this proposal will stage, including a deferred
    /// broadcast and both full agreements. Recovery and signature acceptance
    /// enforce the same rule as dispatch so no signer becomes bound to a
    /// proposal whose successor cannot fit in a publishable receipt.
    fn check_proposal_receipt_budget(
        &self,
        proposal: &SharedProposal,
    ) -> Result<(), ProtocolError> {
        if let Some(successor) = self.deferred_broadcast_successor(proposal)? {
            self.check_receipt_entries(&[&proposal.entry, &successor.entry])?;
        } else {
            self.check_receipt_budget(&proposal.entry)?;
        }
        Ok(())
    }

    fn check_receipt_entries(&self, entries: &[&TraceEntry]) -> Result<u64, ProtocolError> {
        let exhausted = || ProtocolError::ReceiptBudgetExhausted {
            step: self.agreed_step,
        };
        let mut trace_bytes = self.trace_bytes;
        for entry in entries {
            let mut certified = (*entry).clone();
            certified.agreement.signers =
                crate::SignerSet::full(self.binding.activation().tickets().len())
                    .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;
            let bytes = borsh::object_length(&certified)
                .map_err(|error| ProtocolError::Serialization(error.to_string()))?
                as u64;
            trace_bytes = trace_bytes.checked_add(bytes).ok_or_else(exhausted)?;
        }
        if self
            .agreed_step
            .checked_add(entries.len() as u64)
            .is_none_or(|count| count > super::MAX_RECEIPT_TRACE_ENTRIES as u64)
            || trace_bytes
                .checked_add(self.receipt_overhead)
                .is_none_or(|total| total > super::MAX_RECEIPT_BYTES as u64)
        {
            return Err(exhausted());
        }
        Ok(trace_bytes)
    }

    /// Return the shared hash at the last agreed step.
    #[must_use]
    pub const fn agreed_state(&self) -> StateHash {
        self.agreed_state
    }

    /// Return the chain link at the last agreed step.
    #[must_use]
    pub const fn agreed_link(&self) -> [u8; 32] {
        self.agreed_link
    }

    /// Return the last agreed step for which React was durably run.
    #[must_use]
    pub const fn last_reacted_step(&self) -> Option<u64> {
        self.last_reacted_step
    }

    /// Return the agreed-step cursor view.
    #[must_use]
    pub const fn step_cursor(&self) -> StepCursor {
        StepCursor::new(self.agreed_step, self.agreed_state, self.agreed_link)
    }

    /// Return the durable execution version.
    #[must_use]
    pub const fn version(&self) -> ExecutionVersion {
        self.version
    }

    /// Borrow the committed opaque shared state payload.
    #[must_use]
    pub const fn shared_state(&self) -> &SharedStateBytes {
        &self.shared_state
    }

    /// Borrow the committed opaque local state payload.
    #[must_use]
    pub const fn local_state(&self) -> &LocalStateBytes {
        &self.local_state
    }

    /// Borrow the pending shared proposal, if one exists.
    #[must_use]
    pub const fn pending_shared(&self) -> Option<&SharedProposal> {
        self.proposal.as_ref()
    }

    /// Borrow the guest-produced terminal outcome projection, if any.
    #[must_use]
    pub fn terminal_outcome(&self) -> Option<&TerminalOutcome> {
        match &self.status {
            ExecutionStatus::Ended { outcome } | ExecutionStatus::Completed { outcome, .. } => {
                Some(outcome)
            }
            _ => None,
        }
    }

    /// Borrow the validated agent-facing JSON outcome projection.
    #[must_use]
    pub fn terminal_outcome_json(&self) -> Option<&[u8]> {
        self.terminal_outcome().map(TerminalOutcome::json)
    }

    /// Activate an execution after its activation has been durably committed.
    ///
    /// Activation is a lifecycle mutation only. The actor still has to inject
    /// `SessionStarted` through the normal dispatch path after this method
    /// succeeds.
    pub fn activate(&mut self) -> Result<(), ProtocolError> {
        if self.status != ExecutionStatus::Activating {
            return Err(ProtocolError::IllegalLifecycle {
                current: self.lifecycle(),
                event: "activate",
            });
        }
        if self.proposal.is_some() {
            return Err(ProtocolError::InvalidCertificate(
                "activating execution cannot retain a proposal".into(),
            ));
        }

        let mut next = self.clone();
        next.status = ExecutionStatus::active();
        next.bump_version()?;
        next.validate_recovered()?;
        *self = next;
        Ok(())
    }

    /// Apply the pure protocol part of one accepted guest dispatch.
    ///
    /// The state decides whether the result installs immediately or stages a
    /// shared proposal, derives the next status and open callout, normalizes
    /// an establishing broadcast, and advances the version. The caller remains
    /// responsible for validating its durable source and atomically persisting
    /// this state, the returned establishing frame, and the original effects.
    ///
    /// The optional frame establishes a normalized broadcast. `callout` is the
    /// one request the program derived from the accepted post-state.
    #[allow(clippy::too_many_arguments)]
    pub fn apply_dispatch(
        &mut self,
        event: &Event<Vec<u8>>,
        shared_state: SharedStateBytes,
        local_state: LocalStateBytes,
        effects: &[Effect],
        terminal_outcome: Option<TerminalOutcome>,
        pending_id: Option<PendingId>,
        callout: Option<CalloutRequest>,
    ) -> Result<Option<(u32, ExecFrame)>, ProtocolError> {
        if self.proposal.is_some() {
            return Err(ProtocolError::SharedProposalExists);
        }
        let event_position = self.event_position;
        let post_state = StateHash::of_shared(&shared_state);
        let indexed_effects = indexed_dispatch_effects(effects)?;
        validate_effects(&indexed_effects)?;
        let lifecycle = dispatch_lifecycle_effect(effects)?;
        if lifecycle.is_some()
            && effects
                .iter()
                .any(|effect| matches!(effect, Effect::SetTimer { .. }))
        {
            return Err(ProtocolError::InvalidTerminalStatus);
        }
        let broadcast_index = effects
            .iter()
            .position(|effect| matches!(effect, Effect::Broadcast { .. }));
        let portable_event = matches!(
            event,
            Event::SessionStarted { .. } | Event::MessageReceived { .. }
        );
        let requires_agreement = portable_event
            || post_state != self.agreed_state
            || broadcast_index.is_some()
            || lifecycle.is_some();
        let reacted_step = reacted_step(self, event)?;
        validate_callout_dispatch(self, event, pending_id)?;

        if !requires_agreement {
            if terminal_outcome.is_some() {
                return Err(ProtocolError::TerminalOutcomeMismatch);
            }
            let status = ExecutionStatus::active();
            let next_callout = next_open_callout(self, event, event_position, &status, callout);
            self.install_dispatch(
                event_position,
                shared_state,
                local_state,
                status,
                next_callout,
                reacted_step,
            )?;
            return Ok(None);
        }

        let (trace_event, proposal_effects, establishing_frame) =
            normalize_proposal_event(self, event, &shared_state, indexed_effects, broadcast_index)?;
        if lifecycle.is_some()
            && proposal_effects
                .iter()
                .any(|(_, effect)| matches!(effect, Effect::Broadcast { .. }))
        {
            return Err(ProtocolError::InvalidTerminalStatus);
        }
        let entry = TraceEntry {
            trace_version: TRACE_FORMAT_VERSION,
            step: self.agreed_step,
            event: trace_event,
            pre_state: self.agreed_state,
            post_state,
            terminal: lifecycle,
            agreement: AggregateAttestation::empty(),
        };
        let commitment =
            StepCommitment::for_entry(self.binding.session_id(), &entry, self.agreed_link);
        let status = proposal_status(&entry, &commitment, terminal_outcome)?;
        let next_callout = next_open_callout(self, event, event_position, &status, callout);
        let proposal = SharedProposal::new(
            commitment,
            entry,
            shared_state,
            local_state,
            proposal_effects,
            event_position,
            status,
            next_callout,
            Vec::new(),
        )?;
        self.stage_proposal(proposal, reacted_step)?;
        Ok(establishing_frame)
    }

    /// Install one accepted dispatch that does not require shared agreement.
    ///
    /// Such a dispatch may replace either memory and may leave a callout open,
    /// but its shared payload must hash to the current agreed shared state. A
    /// dispatch that changes that hash belongs in [`Self::stage_proposal`].
    /// `reacted_step` records a `React` dispatch in the same durable mutation;
    /// ordinary events pass `None`.
    fn install_dispatch(
        &mut self,
        event_position: u64,
        shared_state: SharedStateBytes,
        local_state: LocalStateBytes,
        status: ExecutionStatus,
        callout: Option<OpenCallout>,
        reacted_step: Option<u64>,
    ) -> Result<(), ProtocolError> {
        if self.proposal.is_some() {
            return Err(ProtocolError::SharedProposalExists);
        }
        if !matches!(self.status, ExecutionStatus::Active) {
            return Err(ProtocolError::IllegalLifecycle {
                current: self.lifecycle(),
                event: "dispatch",
            });
        }
        if !matches!(status, ExecutionStatus::Active) {
            return Err(ProtocolError::InvalidTerminalStatus);
        }
        if event_position != self.event_position {
            return Err(ProtocolError::InvalidCertificate(
                "dispatch event position is not the next event position".into(),
            ));
        }
        if StateHash::of_shared(&shared_state) != self.agreed_state {
            return Err(ProtocolError::StateHashMismatch);
        }
        if let Some(step) = reacted_step
            && (step >= self.agreed_step
                || self
                    .last_reacted_step
                    .is_some_and(|previous| step <= previous))
        {
            return Err(ProtocolError::InvalidTerminalStatus);
        }
        let next_event_position = event_position
            .checked_add(1)
            .ok_or(ProtocolError::VersionExhausted)?;

        let mut next = self.clone();
        next.event_position = next_event_position;
        next.shared_state = shared_state;
        next.local_state = local_state;
        next.status = status;
        next.callout = callout;
        if let Some(step) = reacted_step {
            next.last_reacted_step = Some(step);
        }
        next.bump_version()?;
        next.validate_recovered()?;
        *self = next;
        Ok(())
    }

    /// Stage one dispatch result for signature collection.
    ///
    /// `reacted_step` records a `React` dispatch that produced this result. It
    /// is consumed atomically with proposal staging so recovery cannot rerun
    /// that reaction; the proposed memories and status remain uncommitted
    /// until the step receives agreement.
    fn stage_proposal(
        &mut self,
        proposal: SharedProposal,
        reacted_step: Option<u64>,
    ) -> Result<(), ProtocolError> {
        if self.proposal.is_some() {
            return Err(ProtocolError::SharedProposalExists);
        }
        if !matches!(self.status, ExecutionStatus::Active) {
            return Err(ProtocolError::IllegalLifecycle {
                current: self.lifecycle(),
                event: "shared proposal",
            });
        }
        if proposal.event_position != self.event_position {
            return Err(ProtocolError::InvalidCertificate(
                "proposal event position is not the next event position".into(),
            ));
        }
        if let Some(step) = reacted_step
            && (step >= self.agreed_step
                || self
                    .last_reacted_step
                    .is_some_and(|previous| step <= previous))
        {
            return Err(ProtocolError::InvalidTerminalStatus);
        }
        let next_event_position = self
            .event_position
            .checked_add(1)
            .ok_or(ProtocolError::VersionExhausted)?;
        validate_proposal(
            &self.binding,
            self.step_cursor(),
            self.event_position,
            &proposal,
        )?;
        self.check_proposal_receipt_budget(&proposal)?;
        let mut next = self.clone();
        next.event_position = next_event_position;
        if let Some(step) = reacted_step {
            next.last_reacted_step = Some(step);
        }
        next.proposal = Some(proposal);
        next.bump_version()?;
        next.validate_recovered()?;
        *self = next;
        Ok(())
    }

    /// Add a step signature and promote the proposal when N-of-N is complete.
    ///
    /// `Ok(None)` retains the proposal with one more signature. `Ok(Some(_))`
    /// means the signature completed the certificate and the returned
    /// proposal contains the committed trace agreement and effect list for
    /// the store to release. Both outcomes advance the aggregate version once.
    pub fn add_step_signature(
        &mut self,
        signature: ParticipantStepSignature,
    ) -> Result<Option<SharedProposal>, ProtocolError> {
        // Defend the staging invariant before retaining any signature.
        self.check_proposal_receipt_budget(
            self.proposal
                .as_ref()
                .ok_or(ProtocolError::SharedProposalMissing)?,
        )?;
        let mut next = self.clone();
        {
            let binding = &next.binding;
            next.proposal
                .as_mut()
                .ok_or(ProtocolError::SharedProposalMissing)?
                .add_signature(binding, signature)?;
        }
        let complete = next.proposal.as_ref().is_some_and(|proposal| {
            proposal.signatures.len() == next.binding.activation().tickets().len()
        });
        if complete {
            let certificate = StepCertificate::from_signatures(
                &next.binding,
                next.proposal
                    .as_ref()
                    .ok_or(ProtocolError::SharedProposalMissing)?,
            )?;
            let committed = next.commit_shared_inner(certificate)?;
            next.bump_version()?;
            next.validate_recovered()?;
            *self = next;
            Ok(Some(committed))
        } else {
            next.bump_version()?;
            next.validate_recovered()?;
            *self = next;
            Ok(None)
        }
    }

    fn commit_shared_inner(
        &mut self,
        certificate: StepCertificate,
    ) -> Result<SharedProposal, ProtocolError> {
        let proposal = self
            .proposal
            .as_ref()
            .ok_or(ProtocolError::SharedProposalMissing)?;
        validate_proposal(
            &self.binding,
            self.step_cursor(),
            proposal.event_position,
            proposal,
        )?;
        if certificate.commitment != proposal.commitment {
            return Err(ProtocolError::InvalidCertificate(
                "step certificate does not match the staged proposal".into(),
            ));
        }
        let participants = self.binding.participant_keys()?;
        if !certificate.agreement.signers.is_full(participants.len()) {
            return Err(ProtocolError::IncompleteProof {
                actual: certificate.agreement.signers.count(),
                expected: participants.len(),
            });
        }
        certificate
            .agreement
            .verify_signatures(
                certificate.commitment.step,
                &certificate.commitment.signing_bytes(),
                &participants.iter().map(|(_, key)| *key).collect::<Vec<_>>(),
            )
            .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;
        let next = self.step_cursor().advance(&certificate.commitment)?;
        let trace_bytes = self.check_receipt_budget(&proposal.entry)?;
        let expected_successor = self.deferred_broadcast_successor(proposal)?;
        // Tripwire for the reservation made when the origin was staged.
        self.check_proposal_receipt_budget(proposal)?;
        let mut committed = self
            .proposal
            .take()
            .ok_or(ProtocolError::SharedProposalMissing)?;
        committed.entry.agreement = certificate.agreement.clone();
        self.agreed_step = next.next_step();
        self.trace_bytes = trace_bytes;
        self.agreed_state = next.state_hash();
        self.agreed_link = next.chain_hash();
        self.shared_state = committed.shared_state.clone();
        self.local_state = committed.local_state.clone();
        self.status = committed.status.clone();
        self.callout = committed.callout.clone();
        self.proposal = expected_successor;
        Ok(committed)
    }

    /// Build the deterministic successor proposal for a retained broadcast.
    ///
    /// A broadcast emitted by an agreed event is the next agreed event, not a
    /// second dispatch. The successor therefore reuses the originating event
    /// position, carries the just-committed memories and status, and has no
    /// effects of its own. The state machine installs this successor itself
    /// when the originating proposal reaches N-of-N agreement.
    fn deferred_broadcast_successor(
        &self,
        proposal: &SharedProposal,
    ) -> Result<Option<SharedProposal>, ProtocolError> {
        let Some(data) = proposal
            .effects
            .iter()
            .find_map(|(_, effect)| match effect {
                Effect::Broadcast { data } => Some(data.as_slice()),
                _ => None,
            })
        else {
            return Ok(None);
        };
        let step = proposal
            .commitment
            .step
            .checked_add(1)
            .ok_or(ProtocolError::AgreedStepExhausted)?;
        let pre_state = proposal.commitment.post_state;
        let event = Event::MessageReceived {
            message_id: MessageId::derive(
                self.binding.session_id(),
                self.producer,
                step,
                pre_state,
                pre_state,
                data,
            ),
            from: self.producer,
            position: step,
            pre_state,
            msg: data.to_vec(),
        };
        let entry = TraceEntry {
            trace_version: TRACE_FORMAT_VERSION,
            step,
            event,
            pre_state,
            post_state: pre_state,
            terminal: None,
            agreement: AggregateAttestation::empty(),
        };
        let commitment = StepCommitment::for_entry(
            self.binding.session_id(),
            &entry,
            proposal.commitment.link_hash(),
        );
        Ok(Some(SharedProposal::new(
            commitment,
            entry,
            proposal.shared_state.clone(),
            proposal.local_state.clone(),
            Vec::new(),
            proposal.event_position,
            proposal.status.clone(),
            proposal.callout.clone(),
            Vec::new(),
        )?))
    }

    /// Accept an authenticated stop at the current agreed cursor.
    pub fn stop(&mut self, occurrence: super::AbortOccurrence) -> Result<(), ProtocolError> {
        occurrence.validate_for_session(self.binding.session_id())?;
        if !self
            .binding
            .activation()
            .tickets()
            .iter()
            .any(|ticket| ticket.data.signer == occurrence.sender())
        {
            return Err(ProtocolError::UnauthenticatedAbort);
        }
        if !occurrence.verify_signature()? {
            return Err(ProtocolError::UnauthenticatedAbort);
        }
        if *occurrence.coordinate() != self.step_cursor() {
            return Err(ProtocolError::InvalidAbortCoordinate);
        }
        if self.status.is_terminal() {
            return Err(ProtocolError::AlreadyTerminal);
        }
        if self.proposal.as_ref().is_some_and(|proposal| {
            proposal
                .signatures()
                .iter()
                .any(|signature| signature.participant() == occurrence.sender())
        }) {
            return Err(ProtocolError::SharedProposalSigned);
        }

        let mut next = self.clone();
        next.proposal = None;
        next.callout = None;
        next.status = ExecutionStatus::stopped(occurrence)?;
        next.bump_version()?;
        next.validate_recovered()?;
        *self = next;
        Ok(())
    }

    /// Publish a validated receipt artifact for terminal execution.
    pub fn publish_receipt(&mut self, artifact: ReceiptArtifact) -> Result<(), ProtocolError> {
        validate_receipt_body(&self.binding, artifact.body())?;
        let body = artifact.body();
        let status = match (&self.status, body.termination()) {
            (ExecutionStatus::Ended { outcome }, crate::ReceiptTermination::Completed) => {
                let last = body
                    .trace()
                    .last()
                    .ok_or(ProtocolError::ReceiptBodyMismatch)?;
                let commitment = StepCommitment::for_entry(
                    self.binding.session_id(),
                    last,
                    body.trace().iter().take(body.trace().len() - 1).fold(
                        crate::CHAIN_START,
                        |link, entry| {
                            StepCommitment::for_entry(self.binding.session_id(), entry, link)
                                .link_hash()
                        },
                    ),
                );
                if body.outcome() != outcome.borsh()
                    || last.step.checked_add(1) != Some(self.agreed_step)
                    || last.post_state != self.agreed_state
                    || commitment.link_hash() != self.agreed_link
                {
                    return Err(ProtocolError::ReceiptBodyMismatch);
                }
                ExecutionStatus::completed(outcome.clone(), artifact.receipt_id())
            }
            (
                ExecutionStatus::Stopped { cause },
                crate::ReceiptTermination::Stopped { cause: body_cause },
            ) if cause == body_cause => {
                ExecutionStatus::stopped_published(cause.clone(), artifact.receipt_id())
            }
            (ExecutionStatus::Completed { .. } | ExecutionStatus::StoppedPublished { .. }, _) => {
                return Err(ProtocolError::TerminalAlreadyPublished);
            }
            _ => return Err(ProtocolError::ReceiptBodyMismatch),
        };

        let mut next = self.clone();
        next.status = status;
        next.callout = None;
        next.bump_version()?;
        next.validate_recovered()?;
        *self = next;
        Ok(())
    }

    /// Decode and validate one persisted aggregate.
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtocolError> {
        #[cfg(feature = "performance-tracing")]
        let performance_enabled = tracing::enabled!(
            target: "arena0::performance",
            tracing::Level::DEBUG
        );
        #[cfg(feature = "performance-tracing")]
        let started = performance_enabled.then(Instant::now);
        #[cfg(feature = "performance-tracing")]
        let encoded_size = performance_enabled.then_some(bytes.len());
        let result = (|| {
            ensure_encoded("execution state", bytes.len(), MAX_EXECUTION_STATE_BYTES)?;
            borsh::from_slice::<Self>(bytes)
                .map_err(|error| ProtocolError::Deserialization(error.to_string()))
        })();

        #[cfg(feature = "performance-tracing")]
        if let (Some(started), Some(encoded_size)) = (started, encoded_size) {
            let elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
            match &result {
                Ok(state) => tracing::debug!(
                    target = "arena0::performance",
                    operation = "execution_state_decode",
                    exec_id = %state.execution_id(),
                    session_id = %state.binding().session_id(),
                    version = state.version().get(),
                    agreed_step = state.agreed_step(),
                    encoded_size,
                    success = true,
                    result_class = "decoded",
                    elapsed_us,
                ),
                Err(_) => tracing::debug!(
                    target = "arena0::performance",
                    operation = "execution_state_decode",
                    encoded_size,
                    success = false,
                    result_class = "decode_error",
                    elapsed_us,
                ),
            }
        }

        result
    }

    /// Encode the aggregate and enforce its total durable bound.
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        let bytes =
            borsh::to_vec(self).map_err(|error| ProtocolError::Serialization(error.to_string()))?;
        ensure_encoded("execution state", bytes.len(), MAX_EXECUTION_STATE_BYTES)?;
        Ok(bytes)
    }

    /// Validate aggregate cross-field invariants after decoding persisted bytes.
    pub(super) fn validate_recovered(&self) -> Result<(), ProtocolError> {
        if !self
            .binding
            .activation
            .tickets()
            .iter()
            .any(|ticket| ticket.data.signer == self.producer)
        {
            return Err(ProtocolError::UnknownParticipant {
                participant: self.producer,
            });
        }
        if StateHash::of_shared(&self.shared_state) != self.agreed_state {
            return Err(ProtocolError::StateHashMismatch);
        }
        if self.agreed_step == 0
            && self.agreed_state != self.binding.activation.offer().data().initial_state
        {
            return Err(ProtocolError::StateHashMismatch);
        }
        if (self.agreed_step == 0) != (self.trace_bytes == 0)
            || self.agreed_step > super::MAX_RECEIPT_TRACE_ENTRIES as u64
            || self
                .trace_bytes
                .checked_add(self.receipt_overhead)
                .is_none_or(|total| total > super::MAX_RECEIPT_BYTES as u64)
        {
            return Err(ProtocolError::ReceiptBudgetExhausted {
                step: self.agreed_step,
            });
        }
        if self
            .last_reacted_step
            .is_some_and(|step| step >= self.agreed_step)
        {
            return Err(ProtocolError::InvalidTerminalStatus);
        }
        self.status
            .validate_binding(&self.binding, self.step_cursor())?;
        if !matches!(self.status, ExecutionStatus::Active) && self.callout.is_some() {
            return Err(ProtocolError::InvalidCalloutState);
        }
        if self.status.is_terminal() && self.proposal.is_some() {
            return Err(ProtocolError::InvalidCertificate(
                "terminal execution cannot retain a shared proposal".into(),
            ));
        }
        if let Some(proposal) = &self.proposal {
            let expected_event_position = proposal
                .event_position
                .checked_add(1)
                .ok_or(ProtocolError::VersionExhausted)?;
            if self.event_position != expected_event_position {
                return Err(ProtocolError::InvalidCertificate(
                    "pending proposal is not paired with the next event position".into(),
                ));
            }
            validate_proposal(
                &self.binding,
                self.step_cursor(),
                proposal.event_position,
                proposal,
            )?;
            self.check_proposal_receipt_budget(proposal)?;
            if !matches!(proposal.status(), ExecutionStatus::Active) && proposal.callout().is_some()
            {
                return Err(ProtocolError::InvalidCalloutState);
            }
        }
        Ok(())
    }
}

fn indexed_dispatch_effects(effects: &[Effect]) -> Result<Vec<(u32, Effect)>, ProtocolError> {
    if effects.len() > MAX_EFFECTS {
        return Err(ProtocolError::CollectionTooLarge {
            kind: "effects",
            actual: effects.len(),
            max: MAX_EFFECTS,
        });
    }
    effects
        .iter()
        .enumerate()
        .map(|(index, effect)| {
            let ordinal = u32::try_from(index).map_err(|_| ProtocolError::CollectionTooLarge {
                kind: "effects",
                actual: effects.len(),
                max: u32::MAX as usize,
            })?;
            Ok((ordinal, effect.clone()))
        })
        .collect()
}

fn dispatch_lifecycle_effect(effects: &[Effect]) -> Result<Option<Effect>, ProtocolError> {
    let mut lifecycle = None;
    for effect in effects {
        if matches!(
            effect,
            Effect::SessionEnd { .. } | Effect::SessionAbort { .. } | Effect::Fail { .. }
        ) {
            if lifecycle.is_some() {
                return Err(ProtocolError::MultipleTerminalEffects);
            }
            lifecycle = Some(effect.clone());
        }
    }
    Ok(lifecycle)
}

fn validate_callout_dispatch(
    state: &ExecutionState,
    event: &Event<Vec<u8>>,
    pending_id: Option<PendingId>,
) -> Result<(), ProtocolError> {
    let answer = matches!(event, Event::InputReceived { .. });
    if !answer {
        if pending_id.is_some() {
            return Err(ProtocolError::CalloutMismatch);
        }
        return Ok(());
    }
    let answer_id = pending_id.ok_or(ProtocolError::CalloutMismatch)?;
    let open = state
        .callout
        .as_ref()
        .ok_or(ProtocolError::CalloutMismatch)?;
    if open.id != answer_id {
        return Err(ProtocolError::CalloutMismatch);
    }
    let Event::InputReceived { callout_index, .. } = event else {
        return Err(ProtocolError::CalloutMismatch);
    };
    if *callout_index != open.callout_index {
        return Err(ProtocolError::CalloutMismatch);
    }
    Ok(())
}

/// Derive the open callout installed with a dispatch result.
///
/// A terminal result has none. Otherwise the program's request keeps the
/// current identity for a non-answer event when it repeats the same index and context, replaces it
/// with a fresh event-position identity when it differs, and withdraws it when
/// the program asks nothing. An accepted answer consumes its identity, even
/// when the resulting question has the same index and context.
fn next_open_callout(
    state: &ExecutionState,
    event: &Event<Vec<u8>>,
    event_position: u64,
    status: &ExecutionStatus,
    callout: Option<CalloutRequest>,
) -> Option<OpenCallout> {
    if !matches!(status, ExecutionStatus::Active) {
        return None;
    }
    let request = callout?;
    if let Some(current) = &state.callout
        && !matches!(event, Event::InputReceived { .. })
        && current.callout_index == request.callout_index
        && current.context == request.context
    {
        return Some(current.clone());
    }
    Some(OpenCallout {
        id: pending_id(state.execution_id, event_position),
        callout_index: request.callout_index,
        context: request.context,
    })
}

fn reacted_step(
    state: &ExecutionState,
    event: &Event<Vec<u8>>,
) -> Result<Option<u64>, ProtocolError> {
    if !matches!(event, Event::React) {
        return Ok(None);
    }
    state
        .agreed_step
        .checked_sub(1)
        .map(Some)
        .ok_or(ProtocolError::InvalidTerminalStatus)
}

type NormalizedProposalEvent = (Event<Vec<u8>>, Vec<(u32, Effect)>, Option<(u32, ExecFrame)>);

fn normalize_proposal_event(
    state: &ExecutionState,
    event: &Event<Vec<u8>>,
    shared: &SharedStateBytes,
    mut proposal_effects: Vec<(u32, Effect)>,
    broadcast_index: Option<usize>,
) -> Result<NormalizedProposalEvent, ProtocolError> {
    if let Some(index) = broadcast_index
        && matches!(
            event,
            Event::InputReceived { .. } | Event::TimerFired { .. } | Event::React
        )
    {
        let Effect::Broadcast { data } = &proposal_effects[index].1 else {
            return Err(ProtocolError::InvalidCertificate(
                "broadcast ordinal does not identify a broadcast".into(),
            ));
        };
        let data = data.clone();
        let ordinal = proposal_effects[index].0;
        proposal_effects.remove(index);
        let post_state = StateHash::of_shared(shared);
        let message_id = MessageId::derive(
            state.binding.session_id(),
            state.producer,
            state.agreed_step,
            state.agreed_state,
            post_state,
            &data,
        );
        let frame = ExecFrame::Message {
            message_id,
            seq: state.agreed_step,
            prestate: state.agreed_state,
            poststate: post_state,
            data: data.clone(),
        };
        return Ok((
            Event::MessageReceived {
                message_id,
                from: state.producer,
                position: state.agreed_step,
                pre_state: state.agreed_state,
                msg: data,
            },
            proposal_effects,
            Some((ordinal, frame)),
        ));
    }
    match event {
        Event::SessionStarted { .. } | Event::MessageReceived { .. } => {
            Ok((event.clone(), proposal_effects, None))
        }
        _ => Err(ProtocolError::InvalidCertificate(
            "shared dispatch requires a portable event or a broadcast".into(),
        )),
    }
}

fn proposal_status(
    entry: &TraceEntry,
    commitment: &StepCommitment,
    terminal_outcome: Option<TerminalOutcome>,
) -> Result<ExecutionStatus, ProtocolError> {
    if let Some(terminal) = &entry.terminal {
        match terminal {
            Effect::SessionEnd {
                outcome: effect_outcome,
            } => {
                let outcome = terminal_outcome.ok_or(ProtocolError::TerminalOutcomeRequired)?;
                if outcome.borsh() != effect_outcome.as_slice() {
                    return Err(ProtocolError::OutcomeProjectionMismatch);
                }
                return Ok(ExecutionStatus::Ended { outcome });
            }
            Effect::SessionAbort { .. } | Effect::Fail { .. } => {
                if terminal_outcome.is_some() {
                    return Err(ProtocolError::TerminalOutcomeMismatch);
                }
                return ExecutionStatus::from_shared_entry(entry, commitment.clone())?
                    .ok_or(ProtocolError::InvalidTerminalStatus);
            }
            _ => return Err(ProtocolError::InvalidTerminalStatus),
        }
    }
    if terminal_outcome.is_some() {
        return Err(ProtocolError::TerminalOutcomeMismatch);
    }
    Ok(ExecutionStatus::active())
}

#[cfg(test)]
mod tests {
    use super::*;

    use arena0_crypto::bls::BlsSecretKey;
    use arena0_crypto::{BlsSignature, NodeKeys, SecretKey, key_binding_message};
    use arena0_program::{ExecutionProfile, JsonBytes, ProgramHash};

    use crate::negotiation::{
        Offer, OfferData, OfferHash, PreparedActivation, Ticket, TicketData, TicketHash,
    };
    use crate::trace::{AggregateAttestation, CHAIN_START, TRACE_FORMAT_VERSION};
    use crate::{
        AbortKind, AbortOccurrence, Ensemble, Event, LocalStateBytes, MessageId, NegotiationId,
        OpenCallout, SharedStateBytes, StateHash, StopCause, TicketAction, TraceEntry, pending_id,
    };

    struct Fixture {
        activation: Activation,
        initial: SharedStateBytes,
        participants: Vec<(PeerId, BlsSecretKey)>,
    }

    impl Fixture {
        fn producer(&self) -> PeerId {
            self.participants[0].0
        }
    }

    fn fixture() -> Fixture {
        fixture_with_initial(SharedStateBytes::try_new(vec![0x10, 0x20]).expect("state"))
    }

    fn fixture_with_initial(initial: SharedStateBytes) -> Fixture {
        let creator_identity = NodeKeys::from_secret(SecretKey::from_bytes([1; 32]));
        let other_identity = NodeKeys::from_secret(SecretKey::from_bytes([2; 32]));
        let mut identities = vec![
            (
                PeerId::from_ed25519(&creator_identity.ed25519_public_key()),
                creator_identity,
                BlsSecretKey::from_seed(&[11; 32]).expect("creator BLS key"),
            ),
            (
                PeerId::from_ed25519(&other_identity.ed25519_public_key()),
                other_identity,
                BlsSecretKey::from_seed(&[12; 32]).expect("peer BLS key"),
            ),
        ];
        identities.sort_by_key(|(peer, _, _)| *peer);

        let negotiation_id = NegotiationId([0x11; 32]);
        let offer_data = OfferData::new(
            negotiation_id,
            0,
            identities[0].0,
            ProgramHash([0x22; 32]),
            ExecutionProfile::current().hash(),
            JsonBytes::try_new(br#"{}"#.to_vec()).expect("valid params"),
            identities.len() as u16,
            StateHash::of_shared(&initial),
            1_000_000,
        )
        .expect("valid offer data");
        let offer_hash = OfferHash::of(&offer_data);
        let tickets = identities
            .iter()
            .map(|(peer, identity, bls)| {
                let execution_bls = bls.public_key();
                let key_binding =
                    bls.sign_binding(&key_binding_message(&offer_hash.0, &peer.0, &execution_bls));
                let data = TicketData::new(
                    negotiation_id,
                    0,
                    *peer,
                    0,
                    TicketAction::Active {
                        execution_bls,
                        key_binding,
                        issued_at_unix_ms: 1,
                        valid_for_ms: 60_000,
                    },
                )
                .expect("valid ticket data");
                Ticket {
                    signature: identity.sign(&data.signing_bytes()),
                    data,
                }
            })
            .collect::<Vec<_>>();
        let ticket_hashes = tickets
            .iter()
            .map(|ticket| TicketHash::of(&ticket.data))
            .collect::<Vec<_>>();
        let offer = Offer::new(offer_data, ticket_hashes).expect("complete offer");
        let prepared = PreparedActivation::new(offer, tickets).expect("prepared activation");
        let activation_message = prepared.activation_data().signing_bytes();
        let activation_signatures = identities
            .iter()
            .map(|(_, _, bls)| bls.sign(&activation_message))
            .collect::<Vec<_>>();
        let activation = Activation::new(
            prepared,
            BlsSignature::aggregate(&activation_signatures).expect("activation aggregate"),
        )
        .expect("valid activation");
        let participants = identities
            .into_iter()
            .map(|(peer, _, bls)| (peer, bls))
            .collect();
        Fixture {
            activation,
            initial,
            participants,
        }
    }

    fn active_state(fixture: &Fixture) -> ExecutionState {
        let mut state = ExecutionState::new(
            ExecId([0x44; 32]),
            fixture.activation.clone(),
            fixture.producer(),
            fixture.initial.clone(),
            LocalStateBytes::try_new(vec![0x90]).expect("bounded local state"),
        )
        .expect("valid execution state");
        state.activate().expect("activation transition");
        state
    }

    fn proposal_for(
        fixture: &Fixture,
        state: &ExecutionState,
        shared_state: SharedStateBytes,
        local_state: LocalStateBytes,
    ) -> SharedProposal {
        let ensemble =
            Ensemble::from_peers(fixture.participants.iter().map(|(peer, _)| *peer).collect())
                .expect("complete ensemble");
        let post_state = StateHash::of_shared(&shared_state);
        let entry = TraceEntry {
            trace_version: TRACE_FORMAT_VERSION,
            step: state.agreed_step(),
            event: if state.agreed_step() == 0 {
                Event::SessionStarted { ensemble }
            } else {
                let msg = vec![0x01];
                Event::MessageReceived {
                    message_id: MessageId::derive(
                        fixture.activation.session_hash(),
                        fixture.producer(),
                        state.agreed_step(),
                        state.agreed_state(),
                        post_state,
                        &msg,
                    ),
                    from: fixture.producer(),
                    position: state.agreed_step(),
                    pre_state: state.agreed_state(),
                    msg,
                }
            },
            pre_state: state.agreed_state(),
            post_state,
            terminal: None,
            agreement: AggregateAttestation::empty(),
        };
        let commitment = StepCommitment::for_entry(
            fixture.activation.session_hash(),
            &entry,
            state.agreed_link(),
        );
        SharedProposal::new(
            commitment,
            entry,
            shared_state,
            local_state,
            Vec::new(),
            state.event_position(),
            ExecutionStatus::active(),
            None,
            Vec::new(),
        )
        .expect("valid proposal")
    }

    #[test]
    fn dispatch_derives_callout_identity_and_withdrawal() {
        let fixture = fixture();
        let mut state = active_state(&fixture);
        let event = Event::TimerFired {
            timer: crate::TimerPayload::unit(),
        };
        let request = CalloutRequest {
            callout_index: 0,
            context: b"null".to_vec(),
        };
        let first_position = state.event_position();
        state
            .apply_dispatch(
                &event,
                state.shared_state().clone(),
                state.local_state().clone(),
                &[],
                None,
                None,
                Some(request.clone()),
            )
            .unwrap();
        let first = state.callout().unwrap().clone();
        assert_eq!(first.id, pending_id(state.execution_id(), first_position));
        assert_eq!(state.lifecycle(), ExecLifecycle::Waiting);
        state
            .apply_dispatch(
                &event,
                state.shared_state().clone(),
                state.local_state().clone(),
                &[],
                None,
                None,
                Some(request.clone()),
            )
            .unwrap();
        assert_eq!(state.callout(), Some(&first));
        let answer_position = state.event_position();
        let answer = Event::InputReceived {
            callout_index: first.callout_index,
            data: vec![],
        };
        state
            .apply_dispatch(
                &answer,
                state.shared_state().clone(),
                state.local_state().clone(),
                &[],
                None,
                Some(first.id),
                Some(request.clone()),
            )
            .unwrap();
        let reasked = state.callout().unwrap().clone();
        assert_eq!(reasked.callout_index, first.callout_index);
        assert_eq!(reasked.context, first.context);
        assert_ne!(reasked.id, first.id);
        assert_eq!(
            reasked.id,
            pending_id(state.execution_id(), answer_position)
        );
        let before_replay = state.clone();
        assert_eq!(
            state.apply_dispatch(
                &answer,
                state.shared_state().clone(),
                state.local_state().clone(),
                &[],
                None,
                Some(first.id),
                Some(request.clone())
            ),
            Err(ProtocolError::CalloutMismatch)
        );
        assert_eq!(state, before_replay);
        for replacement in [
            CalloutRequest {
                callout_index: 1,
                ..request
            },
            CalloutRequest {
                callout_index: 1,
                context: b"true".to_vec(),
            },
        ] {
            let previous = state.callout().unwrap().id;
            let position = state.event_position();
            state
                .apply_dispatch(
                    &event,
                    state.shared_state().clone(),
                    state.local_state().clone(),
                    &[],
                    None,
                    None,
                    Some(replacement),
                )
                .unwrap();
            assert_ne!(state.callout().unwrap().id, previous);
            assert_eq!(
                state.callout().unwrap().id,
                pending_id(state.execution_id(), position)
            );
        }
        state
            .apply_dispatch(
                &event,
                state.shared_state().clone(),
                state.local_state().clone(),
                &[],
                None,
                None,
                None,
            )
            .unwrap();
        assert!(state.callout().is_none());
        assert_eq!(state.lifecycle(), ExecLifecycle::Active);
    }

    #[test]
    fn input_requires_the_exact_open_callout() {
        let fixture = fixture();
        let mut state = active_state(&fixture);
        let timer = Event::TimerFired {
            timer: crate::TimerPayload::unit(),
        };
        state
            .apply_dispatch(
                &timer,
                state.shared_state().clone(),
                state.local_state().clone(),
                &[],
                None,
                None,
                Some(CalloutRequest {
                    callout_index: 2,
                    context: vec![],
                }),
            )
            .unwrap();
        let id = state.callout().unwrap().id;
        for (event, answer_id) in [
            (
                Event::InputReceived {
                    callout_index: 2,
                    data: vec![],
                },
                None,
            ),
            (
                Event::InputReceived {
                    callout_index: 2,
                    data: vec![],
                },
                Some(PendingId::new(id.get().wrapping_add(1))),
            ),
            (
                Event::InputReceived {
                    callout_index: 1,
                    data: vec![],
                },
                Some(id),
            ),
            (timer, Some(id)),
        ] {
            let before = state.clone();
            assert_eq!(
                state.apply_dispatch(
                    &event,
                    state.shared_state().clone(),
                    state.local_state().clone(),
                    &[],
                    None,
                    answer_id,
                    None
                ),
                Err(ProtocolError::CalloutMismatch)
            );
            assert_eq!(state, before);
        }
        state
            .apply_dispatch(
                &Event::InputReceived {
                    callout_index: 2,
                    data: vec![],
                },
                state.shared_state().clone(),
                state.local_state().clone(),
                &[],
                None,
                Some(id),
                None,
            )
            .unwrap();
        assert!(state.callout().is_none());
    }

    #[test]
    fn receipt_budget_refuses_the_next_step_without_changing_the_certified_prefix() {
        let fixture = fixture();
        let mut state = active_state(&fixture);
        let mut encoded_trace_bytes = 0;
        loop {
            let mut event = proposal_for(
                &fixture,
                &state,
                state.shared_state().clone(),
                state.local_state().clone(),
            )
            .entry()
            .event
            .clone();
            if let Event::MessageReceived {
                message_id,
                from,
                position,
                pre_state,
                msg,
            } = &mut event
            {
                *msg = vec![1; super::super::MAX_EFFECT_PAYLOAD_BYTES];
                *message_id = MessageId::derive(
                    fixture.activation.session_hash(),
                    *from,
                    *position,
                    *pre_state,
                    state.agreed_state(),
                    msg,
                );
            }
            let before = state.clone();
            let result = state.apply_dispatch(
                &event,
                state.shared_state().clone(),
                state.local_state().clone(),
                &[],
                None,
                None,
                None,
            );
            if let Err(ProtocolError::ReceiptBudgetExhausted { step }) = result {
                assert_eq!(step, before.agreed_step());
                assert_eq!(state, before);
                assert!(state.agreed_step() > 1);
                // The small originating event fits on its own, but its
                // deferred broadcast would not. Refuse the entire dispatch
                // before either entry can be staged or signed.
                let small_event = proposal_for(
                    &fixture,
                    &state,
                    state.shared_state().clone(),
                    state.local_state().clone(),
                )
                .entry()
                .event
                .clone();
                let mut without_broadcast = state.clone();
                without_broadcast
                    .apply_dispatch(
                        &small_event,
                        state.shared_state().clone(),
                        state.local_state().clone(),
                        &[],
                        None,
                        None,
                        None,
                    )
                    .expect("originating entry still fits");
                assert!(matches!(
                    state.apply_dispatch(
                        &small_event,
                        state.shared_state().clone(),
                        state.local_state().clone(),
                        &[Effect::Broadcast {
                            data: vec![1; super::super::MAX_EFFECT_PAYLOAD_BYTES],
                        }],
                        None,
                        None,
                        None,
                    ),
                    Err(ProtocolError::ReceiptBudgetExhausted { step })
                        if step == before.agreed_step()
                ));
                assert_eq!(state, before);
                assert_recovery_rejects_over_budget_broadcast(&fixture, &state, &small_event);
                // A terminal step is subject to the same check; it cannot
                // certify an outcome that leaves local assembly impossible.
                assert!(matches!(
                    state.apply_dispatch(
                        &event,
                        state.shared_state().clone(),
                        state.local_state().clone(),
                        &[Effect::SessionEnd { outcome: vec![] }],
                        Some(TerminalOutcome::new(vec![], b"null".to_vec()).unwrap()),
                        None,
                        None,
                    ),
                    Err(ProtocolError::ReceiptBudgetExhausted { .. })
                ));
                assert_eq!(state, before);
                assert_eq!(
                    ExecutionState::decode(&state.encode().unwrap()).unwrap(),
                    state
                );
                break;
            }
            result.unwrap();
            let commitment = state.pending_shared().unwrap().commitment().clone();
            for (peer, key) in &fixture.participants {
                if let Some(committed) = state
                    .add_step_signature(ParticipantStepSignature::new(
                        *peer,
                        commitment.step,
                        key.sign(&commitment.signing_bytes()),
                    ))
                    .unwrap()
                {
                    encoded_trace_bytes += borsh::object_length(committed.entry()).unwrap() as u64;
                }
            }
            assert_eq!(state.trace_bytes(), encoded_trace_bytes);
            assert!(state.pending_shared().is_none());
        }
    }

    fn assert_recovery_rejects_over_budget_broadcast(
        fixture: &Fixture,
        prefix: &ExecutionState,
        event: &Event<Vec<u8>>,
    ) {
        let mut state = prefix.clone();
        state
            .apply_dispatch(
                event,
                state.shared_state().clone(),
                state.local_state().clone(),
                &[Effect::Broadcast { data: vec![1] }],
                None,
                None,
                None,
            )
            .expect("small deferred broadcast fits");
        assert_eq!(
            ExecutionState::decode(&state.encode().unwrap()).unwrap(),
            state
        );
        assert_eq!(
            serde_json::from_slice::<ExecutionState>(&serde_json::to_vec(&state).unwrap()).unwrap(),
            state
        );

        // Corrupt only the retained payload. The certified prefix and its
        // byte counter remain authentic; the missing reservation is the
        // sole reason both aggregate decode boundaries must reject it.
        state.proposal.as_mut().unwrap().effects[0].1 = Effect::Broadcast {
            data: vec![1; super::super::MAX_EFFECT_PAYLOAD_BYTES],
        };
        let expected_error = format!("receipt budget exhausted at step {}", state.agreed_step());
        let borsh_error = ExecutionState::decode(&state.encode().unwrap()).unwrap_err();
        assert!(borsh_error.to_string().contains(&expected_error));
        let serde_error =
            serde_json::from_slice::<ExecutionState>(&serde_json::to_vec(&state).unwrap())
                .unwrap_err();
        assert!(serde_error.to_string().contains(&expected_error));

        // Even an invalid in-memory aggregate must fail before retaining
        // the first signature, leaving its signer free to authenticate Fail.
        let before = state.clone();
        let commitment = state.pending_shared().unwrap().commitment();
        let (peer, key) = &fixture.participants[0];
        let signature = ParticipantStepSignature::new(
            *peer,
            commitment.step,
            key.sign(&commitment.signing_bytes()),
        );
        assert!(matches!(
            state.add_step_signature(signature),
            Err(ProtocolError::ReceiptBudgetExhausted { step }) if step == prefix.agreed_step()
        ));
        assert_eq!(state, before);
        assert!(state.pending_shared().unwrap().signatures().is_empty());
    }

    #[test]
    fn certified_session_end_is_complete_receipt_evidence() {
        let fixture = fixture();
        let mut state = active_state(&fixture);
        let ensemble =
            Ensemble::from_peers(fixture.participants.iter().map(|(peer, _)| *peer).collect())
                .unwrap();
        let outcome = TerminalOutcome::new(vec![7], b"7".to_vec()).unwrap();
        let mut trace = Vec::new();
        for (event, effects, projection) in [
            (Event::SessionStarted { ensemble }, vec![], None),
            (
                Event::React,
                vec![
                    Effect::Broadcast { data: vec![1] },
                    Effect::SessionEnd { outcome: vec![7] },
                ],
                Some(outcome.clone()),
            ),
        ] {
            state
                .apply_dispatch(
                    &event,
                    state.shared_state().clone(),
                    state.local_state().clone(),
                    &effects,
                    projection,
                    None,
                    None,
                )
                .unwrap();
            // Even an unchanged shared image requires agreement for SessionEnd.
            assert_eq!(state.status(), &ExecutionStatus::Active);
            let commitment = state.pending_shared().unwrap().commitment().clone();
            for (index, (peer, key)) in fixture.participants.iter().enumerate() {
                let committed = state
                    .add_step_signature(ParticipantStepSignature::new(
                        *peer,
                        commitment.step,
                        key.sign(&commitment.signing_bytes()),
                    ))
                    .unwrap();
                if index + 1 < fixture.participants.len() {
                    assert!(committed.is_none());
                    assert_eq!(state.status(), &ExecutionStatus::Active);
                }
                if let Some(committed) = committed {
                    trace.push(committed.entry().clone());
                }
            }
        }
        assert_eq!(
            state.status(),
            &ExecutionStatus::Ended {
                outcome: outcome.clone()
            }
        );
        assert_eq!(state.lifecycle(), ExecLifecycle::Active);
        assert!(matches!(
            state.status().receipt_work(),
            super::super::ReceiptWork::Assemble
        ));
        assert!(state.pending_shared().is_none());
        assert_eq!(
            ExecutionState::decode(&state.encode().unwrap()).unwrap(),
            state
        );

        let body = |entries: Vec<TraceEntry>, bytes: Vec<u8>| {
            crate::ReceiptBody::new(
                crate::SessionHeader::new(
                    fixture.activation.clone(),
                    crate::ReceiptTermination::Completed,
                ),
                bytes,
                fixture.activation.offer().data().params.as_bytes().to_vec(),
                entries,
            )
            .unwrap()
        };
        let artifact = ReceiptArtifact::new(body(trace.clone(), vec![7])).unwrap();
        assert!(ReceiptArtifact::new(body(trace.clone(), vec![8])).is_err());
        assert!(ReceiptArtifact::new(body(trace[..1].to_vec(), vec![7])).is_err());
        let mut uncertified = trace.clone();
        uncertified.last_mut().unwrap().agreement = AggregateAttestation::empty();
        assert!(ReceiptArtifact::new(body(uncertified, vec![7])).is_err());

        // A validly signed different terminal kind cannot claim completion.
        let mut stopped = trace.clone();
        stopped.last_mut().unwrap().terminal = Some(Effect::SessionAbort {
            reason: "stop".into(),
        });
        let previous = StepCommitment::for_entry(
            fixture.activation.session_hash(),
            &stopped[0],
            crate::CHAIN_START,
        );
        let commitment = StepCommitment::for_entry(
            fixture.activation.session_hash(),
            &stopped[1],
            previous.link_hash(),
        );
        let signatures = fixture
            .participants
            .iter()
            .map(|(_, key)| key.sign(&commitment.signing_bytes()))
            .collect::<Vec<_>>();
        stopped[1].agreement = AggregateAttestation::from_signatures(
            crate::SignerSet::full(fixture.participants.len()).unwrap(),
            &signatures,
        )
        .unwrap();
        assert!(ReceiptArtifact::new(body(stopped, vec![7])).is_err());

        state.publish_receipt(artifact.clone()).unwrap();
        assert_eq!(
            state.status(),
            &ExecutionStatus::Completed {
                outcome,
                receipt_id: artifact.receipt_id(),
            }
        );
        assert_eq!(state.lifecycle(), ExecLifecycle::Completed);
    }

    #[test]
    fn certification_installs_callout_and_terminal_clears_it() {
        let fixture = fixture();
        let mut state = active_state(&fixture);
        let ensemble =
            Ensemble::from_peers(fixture.participants.iter().map(|(peer, _)| *peer).collect())
                .unwrap();
        let request = CalloutRequest {
            callout_index: 0,
            context: b"null".to_vec(),
        };
        state
            .apply_dispatch(
                &Event::SessionStarted { ensemble },
                state.shared_state().clone(),
                state.local_state().clone(),
                &[],
                None,
                None,
                Some(request.clone()),
            )
            .unwrap();
        assert!(state.callout().is_none());
        let proposal = state.pending_shared().unwrap().clone();
        assert!(proposal.callout().is_some());
        for (peer, key) in &fixture.participants {
            state
                .add_step_signature(ParticipantStepSignature::new(
                    *peer,
                    proposal.commitment().step,
                    key.sign(&proposal.commitment().signing_bytes()),
                ))
                .unwrap();
        }
        assert_eq!(state.callout(), proposal.callout());
        let open = state.callout().cloned();
        let outcome = TerminalOutcome::new(vec![], b"null".to_vec()).unwrap();
        let event = proposal_for(
            &fixture,
            &state,
            state.shared_state().clone(),
            state.local_state().clone(),
        )
        .entry()
        .event
        .clone();
        state
            .apply_dispatch(
                &event,
                state.shared_state().clone(),
                state.local_state().clone(),
                &[Effect::SessionEnd { outcome: vec![] }],
                Some(outcome),
                None,
                Some(request),
            )
            .unwrap();
        assert_eq!(state.callout(), open.as_ref());
        let proposal = state.pending_shared().unwrap().clone();
        assert!(proposal.callout().is_none());
        for (peer, key) in &fixture.participants {
            state
                .add_step_signature(ParticipantStepSignature::new(
                    *peer,
                    proposal.commitment().step,
                    key.sign(&proposal.commitment().signing_bytes()),
                ))
                .unwrap();
        }
        assert!(state.callout().is_none());
        state.callout = open;
        assert_eq!(
            state.validate_recovered(),
            Err(ProtocolError::InvalidCalloutState)
        );
    }

    #[test]
    fn genesis_hash_uses_the_canonical_shared_memory_image() {
        let fixture = fixture();
        let image = arena0_program::canonical_state_image(fixture.initial.as_bytes())
            .expect("canonical image");
        let state = ExecutionState::new(
            ExecId([0x44; 32]),
            fixture.activation.clone(),
            fixture.producer(),
            fixture.initial.clone(),
            LocalStateBytes::try_new(Vec::new()).expect("bounded local state"),
        )
        .expect("valid execution state");

        assert_eq!(state.agreed_state(), StateHash::of(&image));
        assert_eq!(state.agreed_state(), StateHash::of_shared(&fixture.initial));
        assert_ne!(
            state.agreed_state(),
            StateHash::of(fixture.initial.as_bytes())
        );
    }

    #[test]
    fn execution_state_with_maximal_memories_and_proposal_round_trips() {
        let initial = SharedStateBytes::try_new(vec![0x11; arena0_program::MAX_SHARED_STATE_BYTES])
            .expect("maximal shared state");
        let fixture = fixture_with_initial(initial.clone());
        let mut state = ExecutionState::new(
            ExecId([0x46; 32]),
            fixture.activation.clone(),
            fixture.producer(),
            initial,
            LocalStateBytes::try_new(vec![0x22; arena0_program::MAX_LOCAL_STATE_BYTES])
                .expect("maximal local state"),
        )
        .expect("valid maximal execution state");
        state.activate().expect("activation transition");
        let proposal = proposal_for(
            &fixture,
            &state,
            SharedStateBytes::try_new(vec![0x33; arena0_program::MAX_SHARED_STATE_BYTES])
                .expect("maximal proposed shared state"),
            LocalStateBytes::try_new(vec![0x44; arena0_program::MAX_LOCAL_STATE_BYTES])
                .expect("maximal proposed local state"),
        );
        state
            .stage_proposal(proposal, None)
            .expect("maximal proposal staging");
        let encoded = state.encode().expect("maximal aggregate encoding");
        assert!(encoded.len() > 16 * 1024 * 1024);
        assert!(encoded.len() <= MAX_EXECUTION_STATE_BYTES);
        assert_eq!(
            ExecutionState::decode(&encoded).expect("maximal aggregate decoding"),
            state
        );
    }

    #[test]
    fn activation_and_dispatch_install_are_checked_and_monotonic() {
        let fixture = fixture();
        let mut state = ExecutionState::new(
            ExecId([0x45; 32]),
            fixture.activation.clone(),
            fixture.producer(),
            fixture.initial.clone(),
            LocalStateBytes::try_new(Vec::new()).expect("bounded local state"),
        )
        .expect("valid execution state");
        assert_eq!(state.status(), &ExecutionStatus::Activating);
        assert_eq!(state.version(), ExecutionVersion::ZERO);
        state.activate().expect("activation transition");
        assert_eq!(state.status(), &ExecutionStatus::Active);
        assert_eq!(state.version(), ExecutionVersion::new(1));

        let pending = OpenCallout {
            id: pending_id(state.execution_id(), state.event_position()),
            callout_index: 0,
            context: b"null".to_vec(),
        };
        let local = LocalStateBytes::try_new(vec![0xa0, 0xa1]).expect("bounded local state");
        state
            .install_dispatch(
                state.event_position(),
                fixture.initial.clone(),
                local.clone(),
                ExecutionStatus::active(),
                Some(pending),
                None,
            )
            .expect("dispatch installation");
        assert_eq!(state.event_position(), 1);
        assert_eq!(state.shared_state(), &fixture.initial);
        assert_eq!(state.local_state(), &local);
        assert!(state.callout().is_some());
        assert_eq!(state.version(), ExecutionVersion::new(2));

        let before = state.clone();
        let error = state
            .install_dispatch(
                state.event_position() + 1,
                fixture.initial.clone(),
                local,
                ExecutionStatus::active(),
                None,
                None,
            )
            .expect_err("wrong event coordinate must be rejected");
        assert!(matches!(error, ProtocolError::InvalidCertificate(_)));
        assert_eq!(state, before);
    }

    #[test]
    fn stop_rejects_a_pending_proposal_after_the_producer_signed_it() {
        assert_stop_with_signed_proposal(true, true, false);
    }

    #[test]
    fn peer_stop_is_accepted_after_only_the_producer_signed() {
        assert_stop_with_signed_proposal(false, true, true);
    }

    #[test]
    fn peer_stop_is_refused_after_that_peer_signed() {
        assert_stop_with_signed_proposal(false, false, false);
    }

    fn assert_stop_with_signed_proposal(
        sender_is_producer: bool,
        signer_is_producer: bool,
        accepted: bool,
    ) {
        let fixture = fixture();
        let mut state = active_state(&fixture);
        let proposal = proposal_for(
            &fixture,
            &state,
            SharedStateBytes::try_new(vec![0x51]).expect("state"),
            LocalStateBytes::try_new(vec![0x52]).expect("state"),
        );
        state
            .stage_proposal(proposal, None)
            .expect("stage proposal");
        let staged = state.pending_shared().expect("pending proposal").clone();
        let (signer, signer_key) = fixture
            .participants
            .iter()
            .find(|(peer, _)| (*peer == fixture.producer()) == signer_is_producer)
            .expect("signer key");
        let signature = ParticipantStepSignature::new(
            *signer,
            staged.commitment().step,
            signer_key.sign(&staged.commitment().signing_bytes()),
        );
        assert!(
            state
                .add_step_signature(signature)
                .expect("staged signature")
                .is_none()
        );

        let identity = [1u8, 2]
            .into_iter()
            .map(|seed| NodeKeys::from_secret(SecretKey::from_bytes([seed; 32])))
            .find(|keys| {
                (PeerId::from_ed25519(&keys.ed25519_public_key()) == fixture.producer())
                    == sender_is_producer
            })
            .expect("sender identity");
        let unsigned = AbortOccurrence::unsigned(
            fixture.activation.session_hash(),
            PeerId::from_ed25519(&identity.ed25519_public_key()),
            AbortKind::Abort,
            1,
            "operator stop",
            state.step_cursor(),
        )
        .expect("abort occurrence");
        let occurrence = unsigned
            .clone()
            .with_signature(identity.sign(&unsigned.signing_bytes().expect("abort bytes")))
            .expect("signed abort");
        let before = state.clone();
        if accepted {
            state.stop(occurrence).expect("accept unsigned peer's stop");
            assert!(state.status().is_terminal());
            assert!(state.pending_shared().is_none());
            assert!(state.callout().is_none());
            assert_eq!(state.step_cursor(), before.step_cursor());
            return;
        }
        assert_eq!(
            state.stop(occurrence).unwrap_err(),
            ProtocolError::SharedProposalSigned
        );
        assert_eq!(state, before);
        assert_eq!(
            state
                .pending_shared()
                .expect("proposal retained")
                .signature_count(),
            1
        );
    }

    #[test]
    fn staging_owns_event_position_and_rejects_malformed_results_atomically() {
        let fixture = fixture();
        let mut state = active_state(&fixture);
        let proposed_shared = SharedStateBytes::try_new(vec![0x31, 0x32]).expect("state");
        let proposed_local = LocalStateBytes::try_new(vec![0x41]).expect("state");
        let mut malformed = proposal_for(
            &fixture,
            &state,
            proposed_shared.clone(),
            proposed_local.clone(),
        );
        malformed.commitment.post_state = StateHash([0xee; 32]);
        let before = state.clone();
        assert!(state.stage_proposal(malformed, None).is_err());
        assert_eq!(state, before);

        let proposal = proposal_for(&fixture, &state, proposed_shared, proposed_local);
        state
            .stage_proposal(proposal.clone(), None)
            .expect("valid proposal");
        assert_eq!(state.event_position(), 1);
        assert_eq!(state.pending_shared(), Some(&proposal));

        let after_stage = state.clone();
        assert_eq!(
            state.stage_proposal(proposal, None).unwrap_err(),
            ProtocolError::SharedProposalExists
        );
        assert_eq!(state, after_stage);
    }

    #[test]
    fn complete_step_signatures_promote_the_staged_result_as_one_state() {
        let fixture = fixture();
        let mut state = active_state(&fixture);
        let proposed_shared = SharedStateBytes::try_new(vec![0x61, 0x62]).expect("state");
        let proposed_local = LocalStateBytes::try_new(vec![0x71]).expect("state");
        state
            .stage_proposal(
                proposal_for(
                    &fixture,
                    &state,
                    proposed_shared.clone(),
                    proposed_local.clone(),
                ),
                None,
            )
            .expect("valid proposal");
        let staged = state.pending_shared().expect("staged proposal").clone();

        let (first_peer, first_key) = &fixture.participants[0];
        let first_signature = ParticipantStepSignature::new(
            *first_peer,
            staged.commitment().step,
            first_key.sign(&staged.commitment().signing_bytes()),
        );
        assert!(
            state
                .add_step_signature(first_signature)
                .expect("first signature")
                .is_none()
        );
        assert_eq!(state.shared_state(), &fixture.initial);
        assert_eq!(state.local_state().as_bytes(), &[0x90]);

        let (second_peer, second_key) = &fixture.participants[1];
        let second_signature = ParticipantStepSignature::new(
            *second_peer,
            staged.commitment().step,
            second_key.sign(&staged.commitment().signing_bytes()),
        );
        let committed = state
            .add_step_signature(second_signature)
            .expect("second signature")
            .expect("N-of-N promotion");
        assert!(state.pending_shared().is_none());
        assert_eq!(state.shared_state(), &proposed_shared);
        assert_eq!(state.local_state(), &proposed_local);
        assert_eq!(state.status(), committed.status());
        assert_eq!(state.agreed_step(), 1);
        assert_eq!(state.agreed_state(), StateHash::of_shared(&proposed_shared));
        assert_eq!(state.agreed_link(), committed.commitment().link_hash());
        assert_eq!(committed.entry().agreement.signers.count(), 2);
        assert!(
            committed
                .entry()
                .agreement
                .signers
                .is_full(fixture.participants.len())
        );
        assert_eq!(committed.entry().entry_hash(), staged.entry().entry_hash());

        let second_shared = SharedStateBytes::try_new(vec![0x63]).expect("state");
        let second_local = LocalStateBytes::try_new(vec![0x73]).expect("state");
        state
            .stage_proposal(
                proposal_for(
                    &fixture,
                    &state,
                    second_shared.clone(),
                    second_local.clone(),
                ),
                Some(0),
            )
            .expect("reaction proposal");
        assert_eq!(state.last_reacted_step(), Some(0));
        let second_staged = state.pending_shared().expect("second proposal").clone();
        for (peer, key) in &fixture.participants {
            let signature = ParticipantStepSignature::new(
                *peer,
                second_staged.commitment().step,
                key.sign(&second_staged.commitment().signing_bytes()),
            );
            state
                .add_step_signature(signature)
                .expect("reaction signature");
        }
        assert_eq!(state.agreed_step(), 2);
        assert_eq!(state.last_reacted_step(), Some(0));
        assert_eq!(state.shared_state(), &second_shared);
        assert_eq!(state.local_state(), &second_local);
    }

    #[test]
    fn completing_a_deferred_broadcast_installs_one_successor_for_the_same_event() {
        let fixture = fixture();
        let mut state = active_state(&fixture);
        let proposed_shared = SharedStateBytes::try_new(vec![0xb1, 0xb2]).expect("state");
        let proposed_local = LocalStateBytes::try_new(vec![0xb3]).expect("state");
        let broadcast = vec![0xb4, 0xb5];
        let mut proposal = proposal_for(
            &fixture,
            &state,
            proposed_shared.clone(),
            proposed_local.clone(),
        );
        proposal.effects = vec![(
            4,
            Effect::Broadcast {
                data: broadcast.clone(),
            },
        )];
        state
            .stage_proposal(proposal, None)
            .expect("proposal with deferred broadcast");
        let successor = state
            .deferred_broadcast_successor(state.pending_shared().unwrap())
            .expect("successor construction")
            .expect("retained broadcast");
        assert_eq!(successor.event_position(), 0);
        assert!(successor.effects().is_empty());
        assert_eq!(successor.shared_state(), &proposed_shared);
        assert_eq!(successor.local_state(), &proposed_local);
        let Event::MessageReceived {
            message_id,
            from,
            position,
            pre_state,
            msg,
        } = &successor.entry().event
        else {
            panic!("deferred broadcast must become a message event");
        };
        assert_eq!(*from, fixture.producer());
        assert_eq!(*position, 1);
        assert_eq!(*pre_state, StateHash::of_shared(&proposed_shared));
        assert_eq!(msg, &broadcast);
        assert_eq!(
            *message_id,
            MessageId::derive(
                fixture.activation.session_hash(),
                fixture.producer(),
                1,
                *pre_state,
                *pre_state,
                &broadcast,
            )
        );

        let staged = state.pending_shared().expect("staged proposal").clone();
        let first = &fixture.participants[0];
        let first_signature = ParticipantStepSignature::new(
            first.0,
            staged.commitment().step,
            first.1.sign(&staged.commitment().signing_bytes()),
        );
        assert!(
            state
                .add_step_signature(first_signature)
                .expect("first signature")
                .is_none()
        );
        let second = &fixture.participants[1];
        let second_signature = ParticipantStepSignature::new(
            second.0,
            staged.commitment().step,
            second.1.sign(&staged.commitment().signing_bytes()),
        );
        let committed = state
            .add_step_signature(second_signature)
            .expect("current agreement")
            .expect("committed current proposal");
        assert_eq!(
            committed.effects(),
            &[(4, Effect::Broadcast { data: broadcast })]
        );
        assert_eq!(state.event_position(), 1);
        assert_eq!(state.agreed_step(), 1);
        assert_eq!(state.pending_shared(), Some(&successor));

        let recovered = ExecutionState::decode(&state.encode().expect("encode successor state"))
            .expect("decode successor state");
        assert_eq!(recovered, state);

        let successor = state.pending_shared().expect("successor proposal").clone();
        for (peer, key) in &fixture.participants {
            let signature = ParticipantStepSignature::new(
                *peer,
                successor.commitment().step,
                key.sign(&successor.commitment().signing_bytes()),
            );
            state
                .add_step_signature(signature)
                .expect("successor signature");
        }
        assert!(state.pending_shared().is_none());
        assert_eq!(state.agreed_step(), 2);
        assert_eq!(state.event_position(), 1);
    }

    #[test]
    fn terminal_proposals_consume_establishing_broadcasts_but_reject_deferred_ones() {
        let fixture = fixture();
        let state = active_state(&fixture);
        let terminal = Effect::SessionAbort {
            reason: "guest stopped".into(),
        };

        let mut terminal_only = proposal_for(
            &fixture,
            &state,
            SharedStateBytes::try_new(vec![0xa1]).expect("state"),
            LocalStateBytes::try_new(vec![0xa2]).expect("state"),
        );
        terminal_only.entry.terminal = Some(terminal.clone());
        terminal_only.effects = vec![(0, terminal.clone())];
        terminal_only.commitment = StepCommitment::for_entry(
            fixture.activation.session_hash(),
            &terminal_only.entry,
            state.agreed_link(),
        );
        terminal_only.status = ExecutionStatus::from_shared_entry(
            &terminal_only.entry,
            terminal_only.commitment.clone(),
        )
        .expect("terminal status construction")
        .expect("abort status");
        validate_proposal(
            state.binding(),
            state.step_cursor(),
            state.event_position(),
            &terminal_only,
        )
        .expect("terminal effect is valid when the establishing broadcast was consumed");

        let mut terminal_with_broadcast = terminal_only.clone();
        terminal_with_broadcast.effects =
            vec![(0, terminal), (1, Effect::Broadcast { data: vec![0xa3] })];
        assert!(matches!(
            validate_proposal(
                state.binding(),
                state.step_cursor(),
                state.event_position(),
                &terminal_with_broadcast,
            ),
            Err(ProtocolError::InvalidCertificate(message))
                if message == "terminal agreed event cannot defer a broadcast"
        ));
    }

    #[test]
    fn incomplete_and_conflicting_certificates_do_not_mutate_state() {
        let fixture = fixture();
        let mut state = active_state(&fixture);
        let staged_shared = SharedStateBytes::try_new(vec![0x81]).expect("state");
        state
            .stage_proposal(
                proposal_for(
                    &fixture,
                    &state,
                    staged_shared,
                    LocalStateBytes::try_new(vec![0x82]).expect("state"),
                ),
                None,
            )
            .expect("valid proposal");
        let staged = state.pending_shared().expect("proposal").clone();
        let (peer, key) = &fixture.participants[0];
        let signature = ParticipantStepSignature::new(
            *peer,
            staged.commitment().step,
            key.sign(&staged.commitment().signing_bytes()),
        );
        assert!(
            state
                .add_step_signature(signature.clone())
                .expect("first signature")
                .is_none()
        );
        assert_eq!(
            state.add_step_signature(signature.clone()).unwrap_err(),
            ProtocolError::DuplicateStepSignature { participant: *peer }
        );
        let conflicting =
            ParticipantStepSignature::new(*peer, staged.commitment().step, BlsSignature([0; 48]));
        assert_eq!(
            state.add_step_signature(conflicting).unwrap_err(),
            ProtocolError::ConflictingStepSignature { participant: *peer }
        );

        let incomplete = StepCertificate::from_signatures(
            state.binding(),
            state.pending_shared().expect("proposal"),
        )
        .unwrap_err();
        assert_eq!(
            incomplete,
            ProtocolError::IncompleteProof {
                actual: 1,
                expected: fixture.participants.len(),
            }
        );
        let before = state.clone();
        let malformed = StepCertificate {
            commitment: staged.commitment().clone(),
            agreement: AggregateAttestation::empty(),
        };
        assert!(matches!(
            state.commit_shared_inner(malformed),
            Err(ProtocolError::IncompleteProof { .. })
        ));
        assert_eq!(state, before);
    }

    #[test]
    fn recovery_rejects_noncanonical_hashes_proposals_and_statuses() {
        let fixture = fixture();

        let mut bad_hash = active_state(&fixture);
        bad_hash.agreed_state = StateHash([0xf1; 32]);
        let encoded = borsh::to_vec(&bad_hash).expect("malformed state encoding");
        assert_eq!(
            bad_hash.validate_recovered().unwrap_err(),
            ProtocolError::StateHashMismatch
        );
        assert_eq!(
            ExecutionState::decode(&encoded).unwrap_err(),
            ProtocolError::Deserialization("shared state hash mismatch".into())
        );

        let mut bad_proposal = active_state(&fixture);
        bad_proposal.proposal = Some(proposal_for(
            &fixture,
            &bad_proposal,
            SharedStateBytes::try_new(vec![0x21]).expect("state"),
            LocalStateBytes::try_new(vec![0x22]).expect("state"),
        ));
        assert!(matches!(
            bad_proposal.validate_recovered(),
            Err(ProtocolError::InvalidCertificate(_))
        ));

        let mut mismatched_entry = active_state(&fixture);
        let mut mismatched = proposal_for(
            &fixture,
            &mismatched_entry,
            SharedStateBytes::try_new(vec![0x23]).expect("state"),
            LocalStateBytes::try_new(vec![0x24]).expect("state"),
        );
        mismatched.entry.pre_state = StateHash([0xf2; 32]);
        mismatched_entry.event_position = 1;
        mismatched_entry.proposal = Some(mismatched);
        assert!(matches!(
            mismatched_entry.validate_recovered(),
            Err(ProtocolError::InvalidCertificate(_))
        ));

        let mut terminal_status = active_state(&fixture);
        let mut terminal = proposal_for(
            &fixture,
            &terminal_status,
            SharedStateBytes::try_new(vec![0x25]).expect("state"),
            LocalStateBytes::try_new(vec![0x26]).expect("state"),
        );
        terminal.entry.terminal = Some(Effect::SessionAbort {
            reason: "invalid status".into(),
        });
        terminal.effects = terminal
            .entry
            .terminal
            .clone()
            .into_iter()
            .map(|effect| (0, effect))
            .collect();
        terminal.commitment = StepCommitment::for_entry(
            fixture.activation.session_hash(),
            &terminal.entry,
            terminal_status.agreed_link(),
        );
        terminal_status.event_position = 1;
        terminal_status.proposal = Some(terminal);
        assert_eq!(
            terminal_status.validate_recovered().unwrap_err(),
            ProtocolError::InvalidTerminalStatus
        );

        let mut bad_status = active_state(&fixture);
        bad_status.status = ExecutionStatus::Stopped {
            cause: StopCause::Shared {
                kind: AbortKind::Abort,
                commitment: StepCommitment {
                    domain: [0; 24],
                    session_id: fixture.activation.session_hash(),
                    step: 0,
                    entry_hash: [0; 32],
                    pre_state: bad_status.agreed_state(),
                    post_state: bad_status.agreed_state(),
                    link: CHAIN_START,
                },
                reason: "bad".into(),
            },
        };
        assert_eq!(
            bad_status.validate_recovered().unwrap_err(),
            ProtocolError::InvalidTerminalStatus
        );
    }
}
