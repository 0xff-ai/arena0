use arena0_program::{LocalStateBytes, SharedStateBytes};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::de::Error as _;
use serde::{Deserialize, Serialize};
#[cfg(feature = "performance-tracing")]
use std::time::Instant;

use crate::exec::ExecLifecycle;
use crate::negotiation::Activation;
use crate::trace::{
    AggregateAttestation, PendingOperation, PendingRecord, StepCommitment, TRACE_FORMAT_VERSION,
    TerminalCommitment, TraceEntry,
};
use crate::{
    Effect, Event, ExecFrame, ExecId, MessageId, OutcomeHash, PeerId, PendingId, StateHash,
    pending_id,
};

use super::{
    ExecutionBinding, ExecutionStatus, ExecutionVersion, MAX_EFFECTS, MAX_EXECUTION_STATE_BYTES,
    MAX_PROOF_SIGNATURES, ParticipantStepSignature, ProtocolError, ReceiptArtifact, ReceiptId,
    StepCursor, TerminalOutcome, TerminalProof, ensure_encoded, validate_effects,
    validate_proposal, validate_receipt_body,
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

/// An activation-bound N-of-N certificate for the successful terminal.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TerminalCertificate {
    pub(crate) commitment: TerminalCommitment,
    pub(crate) agreement: AggregateAttestation,
}

impl TerminalCertificate {
    /// Borrow the terminal commitment.
    #[must_use]
    pub const fn commitment(&self) -> &TerminalCommitment {
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
    pub(crate) agreed_state: StateHash,
    pub(crate) agreed_link: [u8; 32],
    pub(crate) last_reacted_step: Option<u64>,
    pub(crate) shared_state: SharedStateBytes,
    pub(crate) local_state: LocalStateBytes,
    pub(crate) proposal: Option<SharedProposal>,
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
    agreed_state: StateHash,
    agreed_link: [u8; 32],
    last_reacted_step: Option<u64>,
    shared_state: SharedStateBytes,
    local_state: LocalStateBytes,
    proposal: Option<SharedProposal>,
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
            agreed_state: self.agreed_state,
            agreed_link: self.agreed_link,
            last_reacted_step: self.last_reacted_step,
            shared_state: self.shared_state.clone(),
            local_state: self.local_state.clone(),
            proposal: self.proposal.clone(),
        }
    }

    fn from_body(body: ExecutionStateBody) -> Result<Self, ProtocolError> {
        let state = Self {
            execution_id: body.execution_id,
            binding: body.binding,
            producer: body.producer,
            status: body.status,
            version: body.version,
            event_position: body.event_position,
            agreed_step: body.agreed_step,
            agreed_state: body.agreed_state,
            agreed_link: body.agreed_link,
            last_reacted_step: body.last_reacted_step,
            shared_state: body.shared_state,
            local_state: body.local_state,
            proposal: body.proposal,
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
            agreed_state: cursor.state_hash(),
            agreed_link: cursor.chain_hash(),
            last_reacted_step: None,
            shared_state,
            local_state,
            proposal: None,
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
    #[must_use]
    pub const fn lifecycle(&self) -> ExecLifecycle {
        self.status.lifecycle()
    }

    /// Borrow the durable execution status.
    #[must_use]
    pub const fn status(&self) -> &ExecutionStatus {
        &self.status
    }

    /// Return the receipt identity retained by a published terminal status.
    #[must_use]
    pub const fn published_receipt_id(&self) -> Option<ReceiptId> {
        match &self.status {
            ExecutionStatus::Completed { proof } => Some(proof.receipt_id),
            ExecutionStatus::StoppedPublished { receipt_id, .. } => Some(*receipt_id),
            ExecutionStatus::Activating
            | ExecutionStatus::Active
            | ExecutionStatus::Waiting { .. }
            | ExecutionStatus::TerminalProof { .. }
            | ExecutionStatus::Stopped { .. }
            | ExecutionStatus::Incomplete { .. } => None,
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

    /// Whether terminal proof has started.
    #[must_use]
    pub fn terminal_pending(&self) -> bool {
        matches!(
            self.status.terminal_proof(),
            Some(TerminalProof::Pending { .. })
        )
    }

    /// Borrow the terminal commitment while signatures are being collected.
    #[must_use]
    pub fn pending_terminal(&self) -> Option<&TerminalCommitment> {
        match self.status.terminal_proof() {
            Some(TerminalProof::Pending { commitment, .. }) => Some(commitment),
            _ => None,
        }
    }

    /// Borrow the terminal certificate once it has been formed.
    #[must_use]
    pub fn terminal_certificate(&self) -> Option<&TerminalCertificate> {
        match self.status.terminal_proof() {
            Some(TerminalProof::Certified { certificate, .. }) => Some(certificate),
            Some(TerminalProof::Pending { .. }) => None,
            None => self
                .status
                .published_proof()
                .map(|proof| &proof.certificate),
        }
    }

    /// Borrow the guest-produced terminal outcome projection, if any.
    #[must_use]
    pub fn terminal_outcome(&self) -> Option<&TerminalOutcome> {
        match self.status.terminal_proof() {
            Some(TerminalProof::Pending { outcome, .. })
            | Some(TerminalProof::Certified { outcome, .. }) => Some(outcome),
            None => self.status.published_proof().map(|proof| &proof.outcome),
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
    /// shared proposal, derives the next status, normalizes an establishing
    /// broadcast, and advances the version. The caller remains responsible
    /// for validating its durable source and atomically persisting this state,
    /// the returned establishing frame, and the original effects.
    ///
    /// The optional frame establishes a normalized broadcast. The boolean is
    /// true only when this dispatch consumes the current callout or signing
    /// continuation instead of retrying it.
    pub fn apply_dispatch(
        &mut self,
        event: &Event<Vec<u8>>,
        shared_state: SharedStateBytes,
        local_state: LocalStateBytes,
        effects: &[Effect],
        terminal_outcome: Option<TerminalOutcome>,
        pending_id: Option<PendingId>,
    ) -> Result<(Option<(u32, ExecFrame)>, bool), ProtocolError> {
        if self.proposal.is_some() {
            return Err(ProtocolError::SharedProposalExists);
        }
        let event_position = self.event_position;
        let post_state = StateHash::of_shared(&shared_state);
        let indexed_effects = indexed_dispatch_effects(effects)?;
        validate_effects(&indexed_effects)?;
        let lifecycle = dispatch_lifecycle_effect(effects)?;
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
        let closes_pending = validate_pending_dispatch(self, event, pending_id, effects)?;

        if !requires_agreement {
            if terminal_outcome.is_some() {
                return Err(ProtocolError::TerminalOutcomeMismatch);
            }
            let status = next_dispatch_status(self, event, event_position, effects)?;
            self.install_dispatch(
                event_position,
                shared_state,
                local_state,
                status,
                reacted_step,
            )?;
            return Ok((None, closes_pending));
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
        let status = proposal_status(
            self,
            event,
            event_position,
            &entry,
            &commitment,
            effects,
            terminal_outcome,
        )?;
        let proposal = SharedProposal::new(
            commitment,
            entry,
            shared_state,
            local_state,
            proposal_effects,
            event_position,
            status,
            Vec::new(),
        )?;
        self.stage_proposal(proposal, reacted_step)?;
        Ok((establishing_frame, closes_pending))
    }

    /// Install one accepted dispatch that does not require shared agreement.
    ///
    /// Such a dispatch may replace either memory and may leave a continuation,
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
        reacted_step: Option<u64>,
    ) -> Result<(), ProtocolError> {
        if self.proposal.is_some() {
            return Err(ProtocolError::SharedProposalExists);
        }
        if !matches!(
            self.status,
            ExecutionStatus::Active | ExecutionStatus::Waiting { .. }
        ) {
            return Err(ProtocolError::IllegalLifecycle {
                current: self.lifecycle(),
                event: "dispatch",
            });
        }
        if !matches!(
            status,
            ExecutionStatus::Active | ExecutionStatus::Waiting { .. }
        ) {
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
        if !matches!(
            self.status,
            ExecutionStatus::Active | ExecutionStatus::Waiting { .. }
        ) {
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
        let expected_successor = self.deferred_broadcast_successor()?;
        let mut committed = self
            .proposal
            .take()
            .ok_or(ProtocolError::SharedProposalMissing)?;
        committed.entry.agreement = certificate.agreement.clone();
        self.agreed_step = next.next_step();
        self.agreed_state = next.state_hash();
        self.agreed_link = next.chain_hash();
        self.shared_state = committed.shared_state.clone();
        self.local_state = committed.local_state.clone();
        self.status = committed.status.clone();
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
    fn deferred_broadcast_successor(&self) -> Result<Option<SharedProposal>, ProtocolError> {
        let proposal = self
            .proposal
            .as_ref()
            .ok_or(ProtocolError::SharedProposalMissing)?;
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
            Vec::new(),
        )?))
    }

    /// Add one checked terminal signature and advance proof progress.
    ///
    /// The returned flag is `true` when the signature completes the N-of-N
    /// certificate. The status owns the proof transition; this method owns the
    /// aggregate version and validates the resulting cross-field state before
    /// publishing it to the caller.
    pub fn add_terminal_signature(
        &mut self,
        signature: super::ParticipantTerminalSignature,
    ) -> Result<bool, ProtocolError> {
        let mut next = self.clone();
        let complete = next
            .status
            .add_terminal_signature(&next.binding, signature)?;
        next.bump_version()?;
        next.validate_recovered()?;
        *self = next;
        Ok(complete)
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
        if self.status.terminal_proof().is_some() {
            return Err(ProtocolError::TerminalProofPending);
        }
        if self.proposal.as_ref().is_some_and(|proposal| {
            proposal
                .signatures()
                .iter()
                .any(|signature| signature.participant() == self.producer)
        }) {
            return Err(ProtocolError::SharedProposalSigned);
        }

        let mut next = self.clone();
        next.proposal = None;
        next.status = ExecutionStatus::stopped(occurrence)?;
        next.bump_version()?;
        next.validate_recovered()?;
        *self = next;
        Ok(())
    }

    /// Freeze in-flight terminal proof when publication is interrupted.
    pub fn interrupt_terminal(&mut self, reason: String) -> Result<(), ProtocolError> {
        if self.status.is_terminal() {
            return Err(ProtocolError::AlreadyTerminal);
        }
        let proof = self
            .status
            .terminal_proof()
            .ok_or(ProtocolError::TerminalProofMissing)?
            .clone();
        let mut next = self.clone();
        next.status = ExecutionStatus::incomplete(proof, reason)?;
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
            (
                ExecutionStatus::TerminalProof { proof },
                crate::ReceiptTermination::Completed { terminal },
            ) => {
                let (certificate, outcome) = proof
                    .certified_parts()
                    .ok_or(ProtocolError::TerminalProofMissing)?;
                if terminal.final_step != certificate.commitment().final_step
                    || terminal.final_state != certificate.commitment().final_state
                    || terminal.outcome_hash != certificate.commitment().outcome_hash
                    || terminal.agreement != *certificate.agreement()
                    || body.outcome() != outcome.borsh()
                {
                    return Err(ProtocolError::ReceiptBodyMismatch);
                }
                ExecutionStatus::completed(
                    certificate.clone(),
                    outcome.clone(),
                    artifact.receipt_id(),
                )
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
        if self
            .last_reacted_step
            .is_some_and(|step| step >= self.agreed_step)
        {
            return Err(ProtocolError::InvalidTerminalStatus);
        }
        self.status
            .validate_binding(&self.binding, self.step_cursor())?;
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

fn validate_pending_dispatch(
    state: &ExecutionState,
    event: &Event<Vec<u8>>,
    pending_id: Option<PendingId>,
    effects: &[Effect],
) -> Result<bool, ProtocolError> {
    let lifecycle = effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::SessionEnd { .. } | Effect::SessionAbort { .. } | Effect::Fail { .. }
        )
    });
    let continuation = effects.iter().any(|effect| {
        matches!(
            effect,
            Effect::Callout { .. }
                | Effect::Sign { .. }
                | Effect::RetryInput { .. }
                | Effect::SetTimer { .. }
        )
    });
    if lifecycle && continuation {
        return Err(ProtocolError::InvalidTerminalStatus);
    }
    let answer = matches!(event, Event::InputReceived { .. } | Event::Signed { .. });
    let retries = effects
        .iter()
        .any(|effect| matches!(effect, Effect::RetryInput { .. }));
    if !answer {
        if retries {
            let Some(current) = state.status.pending() else {
                return Err(ProtocolError::PendingContinuationMismatch);
            };
            if !matches!(current.operation, PendingOperation::Callout { .. })
                || pending_id.is_some_and(|id| id != current.id)
            {
                return Err(ProtocolError::PendingContinuationMismatch);
            }
            return Ok(false);
        }
        if pending_id.is_some() {
            return Err(ProtocolError::PendingContinuationMismatch);
        }
        return Ok(false);
    }
    let Some(pending_id) = pending_id else {
        return Err(ProtocolError::PendingContinuationMismatch);
    };
    let Some(current) = state.status.pending() else {
        return Err(ProtocolError::PendingContinuationMismatch);
    };
    if current.id != pending_id {
        return Err(ProtocolError::PendingContinuationMismatch);
    }
    let matches = match (event, current.operation) {
        (
            Event::InputReceived { callout_index, .. },
            PendingOperation::Callout {
                callout_index: expected_index,
            },
        ) => *callout_index == expected_index,
        (Event::Signed { .. }, PendingOperation::Sign) => true,
        _ => false,
    };
    if !matches {
        return Err(ProtocolError::PendingContinuationMismatch);
    }
    Ok(!retries)
}

fn next_dispatch_status(
    state: &ExecutionState,
    event: &Event<Vec<u8>>,
    event_position: u64,
    effects: &[Effect],
) -> Result<ExecutionStatus, ProtocolError> {
    let existing = state.status.pending().cloned();
    let continuation = effects
        .iter()
        .enumerate()
        .filter(|(_, effect)| matches!(effect, Effect::Callout { .. } | Effect::Sign { .. }))
        .map(|(index, effect)| {
            let ordinal =
                u32::try_from(index).map_err(|_| ProtocolError::InvalidPendingContinuation)?;
            let id = pending_id(state.execution_id, event_position, ordinal);
            PendingRecord::from_effect(id, effect).ok_or(ProtocolError::InvalidPendingContinuation)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if continuation.len() > 1 {
        return Err(ProtocolError::InvalidPendingContinuation);
    }
    let consumes_pending = matches!(event, Event::InputReceived { .. } | Event::Signed { .. });
    let retries = effects
        .iter()
        .any(|effect| matches!(effect, Effect::RetryInput { .. }));
    if let Some(next) = continuation.into_iter().next() {
        if retries || (existing.is_some() && !consumes_pending) {
            return Err(ProtocolError::InvalidPendingContinuation);
        }
        return Ok(ExecutionStatus::waiting(next));
    }
    if retries {
        return existing
            .map(ExecutionStatus::waiting)
            .ok_or(ProtocolError::PendingContinuationMismatch);
    }
    if !consumes_pending {
        return Ok(existing.map_or_else(ExecutionStatus::active, ExecutionStatus::waiting));
    }
    Ok(ExecutionStatus::active())
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
            Event::InputReceived { .. }
                | Event::TimerFired { .. }
                | Event::Signed { .. }
                | Event::React
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

#[allow(clippy::too_many_arguments)]
fn proposal_status(
    state: &ExecutionState,
    event: &Event<Vec<u8>>,
    event_position: u64,
    entry: &TraceEntry,
    commitment: &StepCommitment,
    effects: &[Effect],
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
                let terminal_commitment = TerminalCommitment::new(
                    state.binding.session_id(),
                    entry.step,
                    entry.post_state,
                    OutcomeHash::of(outcome.borsh()),
                );
                return Ok(ExecutionStatus::from_terminal_proof(
                    TerminalProof::pending(terminal_commitment, outcome, Vec::new()),
                ));
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
    next_dispatch_status(state, event, event_position, effects)
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
        PendingOperation, PendingRecord, SharedStateBytes, StateHash, StopCause, TicketAction,
        TraceEntry, pending_id,
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
            Vec::new(),
        )
        .expect("valid proposal")
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

        let pending = PendingRecord {
            id: pending_id(state.execution_id(), state.event_position(), 0),
            operation: PendingOperation::Callout { callout_index: 0 },
            expected_type: Some("bytes".into()),
        };
        let local = LocalStateBytes::try_new(vec![0xa0, 0xa1]).expect("bounded local state");
        state
            .install_dispatch(
                state.event_position(),
                fixture.initial.clone(),
                local.clone(),
                ExecutionStatus::waiting(pending),
                None,
            )
            .expect("dispatch installation");
        assert_eq!(state.event_position(), 1);
        assert_eq!(state.shared_state(), &fixture.initial);
        assert_eq!(state.local_state(), &local);
        assert!(state.status().pending().is_some());
        assert_eq!(state.version(), ExecutionVersion::new(2));

        let before = state.clone();
        let error = state
            .install_dispatch(
                state.event_position() + 1,
                fixture.initial.clone(),
                local,
                ExecutionStatus::active(),
                None,
            )
            .expect_err("wrong event coordinate must be rejected");
        assert!(matches!(error, ProtocolError::InvalidCertificate(_)));
        assert_eq!(state, before);
    }

    #[test]
    fn stop_rejects_a_pending_proposal_after_the_producer_signed_it() {
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
        let (producer, producer_key) = fixture
            .participants
            .iter()
            .find(|(peer, _)| *peer == fixture.producer())
            .expect("producer key");
        let signature = ParticipantStepSignature::new(
            *producer,
            staged.commitment().step,
            producer_key.sign(&staged.commitment().signing_bytes()),
        );
        assert!(
            state
                .add_step_signature(signature)
                .expect("local signature")
                .is_none()
        );

        let identity = [1u8, 2]
            .into_iter()
            .map(|seed| NodeKeys::from_secret(SecretKey::from_bytes([seed; 32])))
            .find(|keys| PeerId::from_ed25519(&keys.ed25519_public_key()) == fixture.producer())
            .expect("producer identity");
        let unsigned = AbortOccurrence::unsigned(
            fixture.activation.session_hash(),
            fixture.producer(),
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
            .deferred_broadcast_successor()
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
