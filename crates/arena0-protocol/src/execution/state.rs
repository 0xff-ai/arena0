use arena0_program::{CalloutRequest, LocalStateBytes, SharedStateBytes};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};
#[cfg(feature = "performance-tracing")]
use std::time::Instant;

use crate::exec::ExecLifecycle;
use crate::negotiation::Activation;
use crate::trace::{
    AggregateAttestation, StepCommitment, StepEvent, StepTerminal, TRACE_FORMAT_VERSION, TraceEntry,
};
use crate::{
    Effect, Event, ExecFrame, ExecId, OpenCallout, PeerId, PendingId, StateHash, pending_id,
};

use super::{
    ExecutionBinding, ExecutionStatus, ExecutionVersion, MAX_EFFECTS, MAX_EXECUTION_STATE_BYTES,
    MAX_PROOF_SIGNATURES, ParticipantStepSignature, ProtocolError, ReceiptArtifact, ReceiptId,
    StepCursor, TerminalOutcome, check_effect_budget, ensure_encoded, ensure_payload,
    validate_effects, validate_proposal, validate_receipt_body,
};

/// A shared step waiting for N-of-N signatures.
///
/// The proposal is the only durable place where a dispatch result can be
/// staged before agreement. It keeps both state memories, the outgoing queue,
/// and the post-agreement timer effects because agreement commits the
/// dispatch atomically: a failed or incomplete agreement must not expose
/// either memory or any queued effect. `event_position` identifies the event
/// that produced the result; it is independent of the agreed trace step.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SharedProposal {
    pub(crate) entry: TraceEntry,
    pub(crate) shared_state: SharedStateBytes,
    pub(crate) local_state: LocalStateBytes,
    /// The outgoing queue this proposal installs when it is certified.
    pub(crate) outgoing: Vec<Vec<u8>>,
    /// Effects paired with their original dispatch ordinals. Broadcasts are
    /// never retained here; every retained timer keeps its original ordinal
    /// for durable timer identity.
    pub(crate) effects: Vec<(u32, Effect)>,
    pub(crate) event_position: u64,
    pub(crate) status: ExecutionStatus,
    /// The open callout this proposal installs when it is certified.
    pub(crate) callout: Option<OpenCallout>,
    pub(crate) signatures: Vec<ParticipantStepSignature>,
}

impl SharedProposal {
    /// Construct a staged dispatch result.
    ///
    /// A non-active status carries no open callout; the commitment is
    /// derived from the entry where it is needed, never stored.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        entry: TraceEntry,
        shared_state: SharedStateBytes,
        local_state: LocalStateBytes,
        outgoing: Vec<Vec<u8>>,
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
        validate_outgoing(&outgoing)?;
        if !matches!(status, ExecutionStatus::Active) && callout.is_some() {
            return Err(ProtocolError::InvalidCalloutState);
        }
        let proposal = Self {
            entry,
            shared_state,
            local_state,
            outgoing,
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

    /// Borrow the outgoing queue this proposal installs when certified.
    #[must_use]
    pub fn outgoing(&self) -> &[Vec<u8>] {
        &self.outgoing
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

    /// Add one participant's checked signature to this proposal.
    ///
    /// The proposal owns the signature list, but the activation binding owns
    /// participant membership and execution keys. Both are required before a
    /// signature is retained. The caller is responsible for persisting the
    /// enclosing [`ExecutionState`] so its version advances exactly once.
    pub fn add_signature(
        &mut self,
        binding: &ExecutionBinding,
        commitment: &StepCommitment,
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
        if signature.signature().step != commitment.step {
            return Err(ProtocolError::InvalidStepSignature {
                participant,
                step: commitment.step,
            });
        }
        let valid = key
            .verify(&commitment.signing_bytes(), &signature.signature().sig)
            .map_err(|error| ProtocolError::InvalidCertificate(error.to_string()))?;
        if !valid {
            return Err(ProtocolError::InvalidStepSignature {
                participant,
                step: commitment.step,
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
#[derive(BorshSerialize, BorshDeserialize, Debug, Clone, PartialEq)]
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
    /// Derived from the binding by construction and by [`Self::decode`]; never persisted.
    #[borsh(skip)]
    receipt_overhead: u64,
    pub(crate) agreed_state: StateHash,
    pub(crate) agreed_link: [u8; 32],
    pub(crate) last_certificate: Option<StepCertificate>,
    pub(crate) end_phase: super::EndPhase,
    /// Local FIFO queue of authored messages. Never part of a commitment,
    /// `StateHash`, or receipt.
    pub(crate) outgoing: Vec<Vec<u8>>,
    pub(crate) shared_state: SharedStateBytes,
    pub(crate) local_state: LocalStateBytes,
    pub(crate) proposal: Option<SharedProposal>,
    pub(crate) callout: Option<OpenCallout>,
}

impl ExecutionState {
    fn next_version(&self) -> Result<ExecutionVersion, ProtocolError> {
        self.version.next().ok_or(ProtocolError::VersionExhausted)
    }

    pub(super) fn bump_version(&mut self) -> Result<(), ProtocolError> {
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
            last_certificate: None,
            end_phase: super::EndPhase::Open,
            outgoing: Vec::new(),
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
            | ExecutionStatus::Certified { .. }
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

    /// Reserve the entry this proposal will stage. Recovery and signature
    /// acceptance enforce the same rule as dispatch so no signer becomes
    /// bound to a proposal that cannot fit in a publishable receipt.
    fn check_proposal_receipt_budget(
        &self,
        proposal: &SharedProposal,
    ) -> Result<(), ProtocolError> {
        self.check_receipt_budget(&proposal.entry)?;
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

    /// Borrow the durable outgoing message queue in FIFO order.
    #[must_use]
    pub fn outgoing(&self) -> &[Vec<u8>] {
        &self.outgoing
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

    /// Derive the staged proposal's commitment, if one exists.
    #[must_use]
    pub fn proposal_commitment(&self) -> Option<StepCommitment> {
        self.proposal
            .as_ref()
            .map(|proposal| self.commitment_for(proposal))
    }

    /// The commitment a staged proposal signs: the session, the entry and the
    /// agreed chain link.
    fn commitment_for(&self, proposal: &SharedProposal) -> StepCommitment {
        StepCommitment::for_entry(self.binding.session_id(), &proposal.entry, self.agreed_link)
    }

    /// Borrow the guest-produced terminal outcome projection, if any.
    #[must_use]
    pub fn terminal_outcome(&self) -> Option<&TerminalOutcome> {
        match &self.status {
            ExecutionStatus::Certified { outcome } | ExecutionStatus::Completed { outcome, .. } => {
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
        // `activate` only flips the lifecycle; the aggregate was valid on
        // entry and no hashed or signed field changed, so no re-check runs.
        *self = next;
        Ok(())
    }

    /// Apply the pure protocol part of one accepted guest dispatch.
    ///
    /// A local event installs immediately and may not change the agreed shared
    /// state. An agreed event always stages a proposal. Broadcasts are
    /// appended to the outgoing queue, never staged as effects. The returned
    /// frame is the establishing message an own-message dispatch delivers.
    /// `callout` is the one request the program derived from the accepted
    /// post-state. The protocol owns the post-state hash: it is computed
    /// here exactly once and used for the local-event check, the entry, and
    /// the installed or staged state.
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
    ) -> Result<Option<ExecFrame>, ProtocolError> {
        if self.proposal.is_some() {
            return Err(ProtocolError::SharedProposalExists);
        }
        let event_position = self.event_position;
        let post_state = StateHash::of_shared(&shared_state);
        let indexed_effects = indexed_dispatch_effects(effects)?;
        let lifecycle = dispatch_lifecycle_effect(effects)?;
        if lifecycle.is_some()
            && effects
                .iter()
                .any(|effect| matches!(effect, Effect::SetTimer { .. }))
        {
            return Err(ProtocolError::InvalidTerminalStatus);
        }
        validate_callout_dispatch(self, event, pending_id)?;
        // The same budget the sandbox enforced at emission; checked again here so release builds never stage or install an over-budget result.
        check_effect_budget(effects)?;
        let agreed_event = matches!(
            event,
            Event::SessionStarted { .. } | Event::MessageReceived { .. }
        );

        if !agreed_event {
            // The sandbox rejects a lifecycle effect from a local event at
            // emission; keep the durable boundary defensive here.
            if lifecycle.is_some() {
                return Err(ProtocolError::InvalidTerminalStatus);
            }
            if post_state != self.agreed_state {
                return Err(ProtocolError::LocalSharedChange);
            }
            if terminal_outcome.is_some() {
                return Err(ProtocolError::TerminalOutcomeMismatch);
            }
            let mut outgoing = self.outgoing.clone();
            append_broadcasts(&mut outgoing, effects)?;
            let status = ExecutionStatus::active();
            let next_callout = next_open_callout(self, event, event_position, &status, callout);
            self.install_dispatch(
                event_position,
                shared_state,
                local_state,
                post_state,
                outgoing,
                status,
                next_callout,
            )?;
            return Ok(None);
        }

        let mut outgoing = self.outgoing.clone();
        // The author dispatches the same two-field event a receiver builds from
        // the frame; the entry is built from the shared coordinates here, so
        // author and receivers produce byte-identical entries by construction.
        let is_own_message =
            matches!(event, Event::MessageReceived { from, .. } if *from == self.producer);
        if is_own_message {
            let Event::MessageReceived { msg, .. } = event else {
                return Err(ProtocolError::InvalidTerminalStatus);
            };
            let Some(head) = outgoing.first() else {
                return Err(ProtocolError::NotQueuedMessage);
            };
            if head.as_slice() != msg.as_slice() {
                return Err(ProtocolError::NotQueuedMessage);
            }
            outgoing.remove(0);
        }
        let trace_event = match event {
            Event::SessionStarted { ensemble } => StepEvent::SessionStarted {
                ensemble: ensemble.clone(),
            },
            Event::MessageReceived { from, msg } => StepEvent::Message {
                from: *from,
                data: msg.clone(),
            },
            Event::InputReceived { .. } | Event::TimerFired { .. } => {
                return Err(ProtocolError::InvalidCertificate(
                    "shared dispatch requires a portable event".into(),
                ));
            }
        };
        append_broadcasts(&mut outgoing, effects)?;
        let proposal_effects = indexed_effects
            .into_iter()
            .filter(|(_, effect)| !matches!(effect, Effect::Broadcast { .. }))
            .collect::<Vec<_>>();
        let entry = TraceEntry {
            trace_version: TRACE_FORMAT_VERSION,
            step: self.agreed_step,
            event: trace_event,
            pre_state: self.agreed_state,
            post_state,
            terminal: lifecycle.as_ref().and_then(StepTerminal::from_effect),
            agreement: AggregateAttestation::empty(),
        };
        let commitment =
            StepCommitment::for_entry(self.binding.session_id(), &entry, self.agreed_link);
        let establishing_frame = if is_own_message {
            let Event::MessageReceived { msg, .. } = event else {
                return Err(ProtocolError::InvalidTerminalStatus);
            };
            Some(ExecFrame::Message {
                commitment: commitment.clone(),
                data: msg.clone(),
            })
        } else {
            None
        };
        let status = proposal_status(&entry, &commitment, terminal_outcome)?;
        let next_callout = next_open_callout(self, event, event_position, &status, callout);
        let proposal = SharedProposal::new(
            entry,
            shared_state,
            local_state,
            outgoing,
            proposal_effects,
            event_position,
            status,
            next_callout,
            Vec::new(),
        )?;
        self.stage_proposal(proposal)?;
        Ok(establishing_frame)
    }

    /// Install one accepted dispatch that does not require shared agreement.
    ///
    /// Such a dispatch may replace local memory and may leave a callout open,
    /// but its shared payload must hash to the current agreed shared state. A
    /// dispatch that changes that hash is rejected by [`Self::apply_dispatch`].
    /// `post_state` is the sandbox-computed hash of `shared_state`.
    #[allow(clippy::too_many_arguments)]
    fn install_dispatch(
        &mut self,
        event_position: u64,
        shared_state: SharedStateBytes,
        local_state: LocalStateBytes,
        post_state: StateHash,
        outgoing: Vec<Vec<u8>>,
        status: ExecutionStatus,
        callout: Option<OpenCallout>,
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
        if post_state != self.agreed_state {
            return Err(ProtocolError::StateHashMismatch);
        }
        // The outgoing queue carries guest-influenced bytes; re-check its
        // length and per-message bounds here since the full re-check no
        // longer runs on this path.
        validate_outgoing(&outgoing)?;
        let next_event_position = event_position
            .checked_add(1)
            .ok_or(ProtocolError::VersionExhausted)?;

        let mut next = self.clone();
        next.event_position = next_event_position;
        next.shared_state = shared_state;
        next.local_state = local_state;
        next.outgoing = outgoing;
        next.status = status;
        next.callout = callout;
        next.bump_version()?;
        *self = next;
        Ok(())
    }

    /// Stage one dispatch result for signature collection.
    ///
    /// The proposed memories, outgoing queue, and status remain uncommitted
    /// until the step receives agreement.
    fn stage_proposal(&mut self, proposal: SharedProposal) -> Result<(), ProtocolError> {
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
        // Staging checks the proposal's own inputs (binding, cursor,
        // receipt budget); the aggregate was valid on entry and no hashed
        // or signed committed field changed, so no re-check runs.
        let mut next = self.clone();
        next.event_position = next_event_position;
        next.proposal = Some(proposal);
        next.bump_version()?;
        *self = next;
        Ok(())
    }

    /// Remove the head of the outgoing queue after its message was rejected by
    /// the program. Errors when the queue is empty or a proposal is staged.
    pub fn drop_outgoing_head(&mut self) -> Result<(), ProtocolError> {
        if self.proposal.is_some() {
            return Err(ProtocolError::SharedProposalExists);
        }
        if self.outgoing.is_empty() {
            return Err(ProtocolError::NotQueuedMessage);
        }
        let mut next = self.clone();
        next.outgoing.remove(0);
        next.bump_version()?;
        // Only the queue shrank; every other invariant is unchanged.
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
        // Derive the commitment once: signature verification and the
        // completion branch below share it instead of re-hashing the entry.
        let commitment = next
            .proposal_commitment()
            .ok_or(ProtocolError::SharedProposalMissing)?;
        {
            let binding = &next.binding;
            next.proposal
                .as_mut()
                .ok_or(ProtocolError::SharedProposalMissing)?
                .add_signature(binding, &commitment, signature)?;
        }
        let complete = next.proposal.as_ref().is_some_and(|proposal| {
            proposal.signatures.len() == next.binding.activation().tickets().len()
        });
        if complete {
            let certificate = StepCertificate::from_signatures(
                &next.binding,
                &commitment,
                next.proposal
                    .as_ref()
                    .ok_or(ProtocolError::SharedProposalMissing)?,
            )?;
            let committed = next.install_certified(certificate)?;
            next.bump_version()?;
            // The commit path verified the certificate and re-checked the
            // proposal budget; no second BLS verification runs here.
            *self = next;
            Ok(Some(committed))
        } else {
            next.bump_version()?;
            // Only the signature list grew; `add_signature` checked the new
            // signature against the staged commitment.
            *self = next;
            Ok(None)
        }
    }

    /// Frames that another participant may still need. Restart resends this
    /// durable evidence without creating new signatures.
    #[must_use]
    pub fn current_frames(&self, me: PeerId) -> Vec<ExecFrame> {
        let mut frames = Vec::new();
        if let Some(certificate) = &self.last_certificate {
            frames.push(ExecFrame::StepCertificate {
                certificate: certificate.clone(),
            });
        }
        if let Some(proposal) = &self.proposal {
            // Derive once per call; the commitment is a pure function of the
            // staged entry and the agreed link.
            let commitment = self.commitment_for(proposal);
            if let StepEvent::Message { from, data } = &proposal.entry.event
                && *from == me
            {
                frames.push(ExecFrame::Message {
                    commitment: commitment.clone(),
                    data: data.clone(),
                });
            }
            if let Some(signature) = proposal
                .signatures
                .iter()
                .find(|signature| signature.participant() == me)
            {
                frames.push(ExecFrame::StepSignature {
                    commitment,
                    signature: signature.signature().sig,
                });
            }
        }
        if let Some(super::StopCause::Authenticated(occurrence)) = self.status.terminal_cause() {
            frames.push(ExecFrame::Abort {
                occurrence: occurrence.clone(),
            });
        }
        frames
    }

    /// Commit a staged proposal using complete N-of-N evidence. The returned
    /// proposal carries the agreed entry and effects for atomic persistence.
    pub fn certify_step(
        &mut self,
        certificate: StepCertificate,
    ) -> Result<SharedProposal, ProtocolError> {
        let mut next = self.clone();
        let committed = next.install_certified(certificate)?;
        next.bump_version()?;
        // Same single-verification rule as the `add_step_signature` commit
        // branch above.
        *self = next;
        Ok(committed)
    }

    fn install_certified(
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
        if certificate.commitment != self.commitment_for(proposal) {
            return Err(ProtocolError::InvalidCertificate(
                "step certificate does not match the staged proposal".into(),
            ));
        }
        certificate.verify(&self.binding)?;
        let next = self.step_cursor().advance(&certificate.commitment)?;
        let trace_bytes = self.check_receipt_budget(&proposal.entry)?;
        // Tripwire for the reservation made when the proposal was staged.
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
        self.last_certificate = Some(certificate);
        self.shared_state = committed.shared_state.clone();
        self.local_state = committed.local_state.clone();
        self.outgoing = committed.outgoing.clone();
        self.status = committed.status.clone();
        self.begin_end();
        self.callout = committed.callout.clone();
        self.proposal = None;
        Ok(committed)
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
        next.begin_end();
        next.bump_version()?;
        // The occurrence was authenticated above and `stopped` builds the
        // terminal status; no hashed or signed field changed.
        *self = next;
        Ok(())
    }

    /// Publish a validated receipt artifact for terminal execution.
    pub fn publish_receipt(&mut self, artifact: ReceiptArtifact) -> Result<(), ProtocolError> {
        validate_receipt_body(&self.binding, artifact.body())?;
        let body = artifact.body();
        let status = match (&self.status, body.termination()) {
            (ExecutionStatus::Certified { outcome }, crate::ReceiptTermination::Completed) => {
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
        // The artifact body was validated against the terminal status above;
        // publication only flips the status.
        *self = next;
        Ok(())
    }

    /// Decode and validate one persisted aggregate. This is
    /// the only way to recover an `ExecutionState` from bytes; raw Borsh
    /// deserialization skips the derived receipt overhead and recovery validation.
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
            let mut state = borsh::from_slice::<Self>(bytes)
                .map_err(|error| ProtocolError::Deserialization(error.to_string()))?;
            state.receipt_overhead = ReceiptArtifact::reserved_overhead(&state.binding)?;
            state.validate_recovered()?;
            Ok(state)
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
        validate_outgoing(&self.outgoing)?;
        self.validate_end()?;
        match &self.last_certificate {
            Some(certificate)
                if certificate.commitment.step.checked_add(1) == Some(self.agreed_step)
                    && certificate.commitment.post_state == self.agreed_state
                    && certificate.commitment.link_hash() == self.agreed_link =>
            {
                certificate.verify(&self.binding)?;
            }
            None if self.agreed_step == 0 => {}
            _ => {
                return Err(ProtocolError::InvalidCertificate(
                    "last certificate does not match the agreed cursor".into(),
                ));
            }
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
            validate_effects(proposal.effects())?;
            validate_proposal(
                &self.binding,
                self.step_cursor(),
                proposal.event_position,
                proposal,
            )?;
            validate_outgoing(proposal.outgoing())?;
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

/// Append every broadcast effect to the durable outgoing queue.
///
/// The sandbox already enforces the queue bound against the committed length;
/// this is the durable boundary check.
fn append_broadcasts(outgoing: &mut Vec<Vec<u8>>, effects: &[Effect]) -> Result<(), ProtocolError> {
    for effect in effects {
        if let Effect::Broadcast { data } = effect {
            if outgoing.len() >= super::MAX_OUTGOING_MESSAGES {
                return Err(ProtocolError::OutgoingQueueFull {
                    actual: outgoing.len(),
                    max: super::MAX_OUTGOING_MESSAGES,
                });
            }
            outgoing.push(data.clone());
        }
    }
    Ok(())
}

fn validate_outgoing(outgoing: &[Vec<u8>]) -> Result<(), ProtocolError> {
    if outgoing.len() > super::MAX_OUTGOING_MESSAGES {
        return Err(ProtocolError::OutgoingQueueFull {
            actual: outgoing.len(),
            max: super::MAX_OUTGOING_MESSAGES,
        });
    }
    for message in outgoing {
        ensure_payload(
            "outgoing message",
            message.len(),
            super::MAX_EFFECT_PAYLOAD_BYTES,
        )?;
    }
    Ok(())
}

fn proposal_status(
    entry: &TraceEntry,
    commitment: &StepCommitment,
    terminal_outcome: Option<TerminalOutcome>,
) -> Result<ExecutionStatus, ProtocolError> {
    if let Some(terminal) = &entry.terminal {
        match terminal {
            StepTerminal::End {
                outcome: effect_outcome,
            } => {
                let outcome = terminal_outcome.ok_or(ProtocolError::TerminalOutcomeRequired)?;
                if outcome.borsh() != effect_outcome.as_slice() {
                    return Err(ProtocolError::OutcomeProjectionMismatch);
                }
                return Ok(ExecutionStatus::Certified { outcome });
            }
            StepTerminal::Abort { .. } | StepTerminal::Fail { .. } => {
                if terminal_outcome.is_some() {
                    return Err(ProtocolError::TerminalOutcomeMismatch);
                }
                return ExecutionStatus::from_shared_entry(entry, commitment.clone())?
                    .ok_or(ProtocolError::InvalidTerminalStatus);
            }
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
        AbortKind, AbortOccurrence, Ensemble, Event, LocalStateBytes, NegotiationId, OpenCallout,
        SharedStateBytes, StateHash, StepEvent, StepTerminal, StopCause, TicketAction, TraceEntry,
        pending_id,
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

    #[test]
    fn end_confirmation_is_local_idempotent_and_retains_silent_peers() {
        use crate::{EndMatch, EndPhase};
        let fixture = fixture();
        let mut state = terminal_state(&fixture, Effect::SessionEnd { outcome: vec![7] });
        let remote = fixture
            .participants
            .iter()
            .map(|(p, _)| *p)
            .find(|p| *p != fixture.producer())
            .unwrap();
        let evidence = state.terminal_evidence().unwrap();
        assert_eq!(state.end_conclusion_matches(&evidence), EndMatch::Same);
        assert!(matches!(state.end_phase(), EndPhase::Ending { .. }));
        let version = state.version();
        state.expire_end().unwrap();
        assert_valid(&state);
        assert!(state.version() > version);
        assert!(
            matches!(state.end_phase(), EndPhase::Ended { unconfirmed } if unconfirmed.contains(&remote))
        );
        assert!(state.expire_end().is_err());
        assert!(state.confirm_end(state.producer()).is_err());
        assert!(state.confirm_end(PeerId([0xff; 32])).is_err());
        assert!(state.confirm_end(remote).unwrap());
        let confirmed = state.clone();
        assert!(!state.confirm_end(remote).unwrap());
        assert_eq!(state, confirmed);
        assert!(
            matches!(state.end_phase(), EndPhase::Ended { unconfirmed } if unconfirmed.is_empty())
        );
        assert_eq!(state.terminal_evidence(), Some(evidence));
        assert_eq!(
            ExecutionState::decode(&state.encode().unwrap()).unwrap(),
            state
        );
        let mut open = active_state(&fixture);
        assert!(open.confirm_end(remote).is_err());
        assert!(open.expire_end().is_err());
    }

    #[test]
    fn completion_and_shared_stops_match_their_final_certificate() {
        for effect in [
            Effect::SessionEnd { outcome: vec![7] },
            Effect::SessionAbort {
                reason: "stop".into(),
            },
            Effect::Fail {
                reason: "failure".into(),
            },
        ] {
            let fixture = fixture();
            let mut state = terminal_state(&fixture, effect);
            let evidence = state.terminal_evidence().unwrap();
            assert!(matches!(evidence, ExecFrame::StepCertificate { .. }));
            assert_eq!(
                state.end_conclusion_matches(&evidence),
                crate::EndMatch::Same
            );
            let remote = fixture
                .participants
                .iter()
                .map(|(p, _)| *p)
                .find(|p| *p != fixture.producer())
                .unwrap();
            state.confirm_end(remote).unwrap();
            assert_valid(&state);
            assert!(
                matches!(state.end_phase(), crate::EndPhase::Ended { unconfirmed } if unconfirmed.is_empty())
            );
        }
    }

    #[test]
    fn confirming_last_remote_peer_completes_ending() {
        let fixture = fixture();
        let mut state = terminal_state(&fixture, Effect::SessionEnd { outcome: vec![7] });
        let remote = fixture
            .participants
            .iter()
            .map(|(peer, _)| *peer)
            .find(|peer| *peer != fixture.producer())
            .unwrap();
        assert!(
            matches!(state.end_phase(), crate::EndPhase::Ending { unconfirmed } if unconfirmed.len() == 1)
        );
        assert!(state.confirm_end(remote).unwrap());
        assert_valid(&state);
        assert!(
            matches!(state.end_phase(), crate::EndPhase::Ended { unconfirmed } if unconfirmed.is_empty())
        );
    }

    /// Decode-time invariant check for tests: every successful transition
    /// must preserve the recovered-state invariants, without paying for the
    /// full re-check (shared re-hash plus certificate BLS verification) at
    /// runtime.
    fn assert_valid(state: &ExecutionState) {
        state
            .validate_recovered()
            .expect("transition preserves recovered-state invariants");
    }

    /// Test stand-in for the dispatch path: run the protocol transition and
    /// prove the result still decodes as valid. A method (not a free
    /// function) so callers can pass `state`-borrowed arguments alongside
    /// the `&mut` receiver, exactly as production callers do.
    impl ExecutionState {
        #[allow(clippy::too_many_arguments)]
        fn sandbox_apply(
            &mut self,
            event: &Event<Vec<u8>>,
            shared: SharedStateBytes,
            local: LocalStateBytes,
            effects: &[Effect],
            outcome: Option<TerminalOutcome>,
            pending_id: Option<PendingId>,
            callout: Option<CalloutRequest>,
        ) -> Result<Option<ExecFrame>, ProtocolError> {
            let result =
                self.apply_dispatch(event, shared, local, effects, outcome, pending_id, callout);
            if result.is_ok() {
                assert_valid(self);
            }
            result
        }
    }

    /// Add every participant signature to the staged proposal and push the
    /// certified entry into `trace`.
    fn certify_pending(state: &mut ExecutionState, fixture: &Fixture, trace: &mut Vec<TraceEntry>) {
        let commitment = state.proposal_commitment().expect("staged proposal");
        for (index, (peer, key)) in fixture.participants.iter().enumerate() {
            let committed = state
                .add_step_signature(ParticipantStepSignature::new(
                    *peer,
                    commitment.step,
                    key.sign(&commitment.signing_bytes()),
                ))
                .unwrap();
            assert_valid(state);
            if index + 1 < fixture.participants.len() {
                assert!(committed.is_none());
                assert_eq!(state.status(), &ExecutionStatus::Active);
            }
            if let Some(committed) = committed {
                trace.push(committed.entry().clone());
            }
        }
    }

    fn terminal_state(fixture: &Fixture, terminal: Effect) -> ExecutionState {
        let mut state = active_state(fixture);
        let ensemble =
            Ensemble::from_peers(fixture.participants.iter().map(|(p, _)| *p).collect()).unwrap();
        let outcome = matches!(terminal, Effect::SessionEnd { .. })
            .then(|| TerminalOutcome::new(vec![7], b"7".to_vec()).unwrap());
        state
            .sandbox_apply(
                &Event::SessionStarted { ensemble },
                state.shared_state().clone(),
                state.local_state().clone(),
                &[terminal],
                outcome,
                None,
                None,
            )
            .unwrap();
        assert_valid(&state);
        let commitment = state.proposal_commitment().expect("staged proposal");
        for (peer, key) in &fixture.participants {
            state
                .add_step_signature(ParticipantStepSignature::new(
                    *peer,
                    commitment.step,
                    key.sign(&commitment.signing_bytes()),
                ))
                .unwrap();
        }
        assert_valid(&state);
        state
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
        assert_valid(&state);
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
        // A remote sender keeps the built entry a peer message rather than an
        // authored own message, which `apply_dispatch` requires to be queued.
        let sender = fixture
            .participants
            .iter()
            .map(|(peer, _)| *peer)
            .find(|peer| *peer != fixture.producer())
            .unwrap_or_else(|| fixture.producer());
        let entry = TraceEntry {
            trace_version: TRACE_FORMAT_VERSION,
            step: state.agreed_step(),
            event: if state.agreed_step() == 0 {
                StepEvent::SessionStarted { ensemble }
            } else {
                StepEvent::Message {
                    from: sender,
                    data: vec![0x01],
                }
            },
            pre_state: state.agreed_state(),
            post_state,
            terminal: None,
            agreement: AggregateAttestation::empty(),
        };
        SharedProposal::new(
            entry,
            shared_state,
            local_state,
            Vec::new(),
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
            .sandbox_apply(
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
            .sandbox_apply(
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
            .sandbox_apply(
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
            state.sandbox_apply(
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
                .sandbox_apply(
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
            .sandbox_apply(
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
            .sandbox_apply(
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
                state.sandbox_apply(
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
            .sandbox_apply(
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

    fn local_timer_event() -> Event<Vec<u8>> {
        Event::TimerFired {
            timer: crate::TimerPayload::unit(),
        }
    }

    /// An active execution whose step-0 `SessionStarted` entry is certified.
    fn started_state(fixture: &Fixture) -> ExecutionState {
        started_state_as(fixture, fixture.producer())
    }

    /// An active execution for `producer` whose step-0 `SessionStarted` entry
    /// is certified.
    fn started_state_as(fixture: &Fixture, producer: PeerId) -> ExecutionState {
        let mut state = ExecutionState::new(
            ExecId([0x44; 32]),
            fixture.activation.clone(),
            producer,
            fixture.initial.clone(),
            LocalStateBytes::try_new(vec![0x90]).expect("bounded local state"),
        )
        .expect("valid execution state");
        state.activate().expect("activation transition");
        let ensemble =
            Ensemble::from_peers(fixture.participants.iter().map(|(peer, _)| *peer).collect())
                .expect("complete ensemble");
        state
            .sandbox_apply(
                &Event::SessionStarted { ensemble },
                state.shared_state().clone(),
                state.local_state().clone(),
                &[],
                None,
                None,
                None,
            )
            .unwrap();
        let commitment = state.proposal_commitment().unwrap().clone();
        for (peer, key) in &fixture.participants {
            state
                .add_step_signature(ParticipantStepSignature::new(
                    *peer,
                    commitment.step,
                    key.sign(&commitment.signing_bytes()),
                ))
                .unwrap();
        }
        assert_valid(&state);
        state
    }

    #[test]
    fn local_dispatch_changing_shared_state_is_rejected() {
        let fixture = fixture();
        let mut state = active_state(&fixture);
        let before = state.clone();
        let changed = SharedStateBytes::try_new(vec![0x7f]).expect("state");
        let error = state
            .sandbox_apply(
                &local_timer_event(),
                changed,
                state.local_state().clone(),
                &[],
                None,
                None,
                None,
            )
            .unwrap_err();
        assert_eq!(error, ProtocolError::LocalSharedChange);
        assert_eq!(state, before);
    }

    #[test]
    fn local_broadcast_is_queued_and_installs_without_a_proposal() {
        let fixture = fixture();
        let mut state = active_state(&fixture);
        let before_step = state.agreed_step();
        let before_position = state.event_position();
        state
            .sandbox_apply(
                &local_timer_event(),
                state.shared_state().clone(),
                state.local_state().clone(),
                &[Effect::Broadcast { data: vec![0xaa] }],
                None,
                None,
                None,
            )
            .unwrap();
        assert!(state.pending_shared().is_none());
        assert_eq!(state.outgoing(), &[vec![0xaa]]);
        assert_eq!(state.agreed_step(), before_step);
        assert_eq!(state.event_position(), before_position + 1);
    }

    #[test]
    fn own_message_must_match_the_outgoing_head() {
        let fixture = fixture();
        let mut state = started_state(&fixture);
        state
            .sandbox_apply(
                &local_timer_event(),
                state.shared_state().clone(),
                state.local_state().clone(),
                &[Effect::Broadcast { data: vec![0xaa] }],
                None,
                None,
                None,
            )
            .unwrap();
        let before = state.clone();
        let mismatched = Event::MessageReceived {
            from: fixture.producer(),
            msg: vec![0xbb],
        };
        assert_eq!(
            state.sandbox_apply(
                &mismatched,
                state.shared_state().clone(),
                state.local_state().clone(),
                &[],
                None,
                None,
                None,
            ),
            Err(ProtocolError::NotQueuedMessage)
        );
        assert_eq!(state, before);

        let matching = Event::MessageReceived {
            from: fixture.producer(),
            msg: vec![0xaa],
        };
        let frame = state
            .sandbox_apply(
                &matching,
                state.shared_state().clone(),
                state.local_state().clone(),
                &[],
                None,
                None,
                None,
            )
            .unwrap()
            .expect("establishing frame");
        assert!(matches!(frame, ExecFrame::Message { data, .. } if data == vec![0xaa]));
        assert!(
            state
                .pending_shared()
                .expect("proposal")
                .outgoing()
                .is_empty()
        );
    }

    #[test]
    fn own_message_stages_outgoing_and_installs_it_at_certification() {
        let fixture = fixture();
        let mut state = started_state(&fixture);
        state
            .sandbox_apply(
                &local_timer_event(),
                state.shared_state().clone(),
                state.local_state().clone(),
                &[Effect::Broadcast { data: vec![0xaa] }],
                None,
                None,
                None,
            )
            .unwrap();
        let matching = Event::MessageReceived {
            from: fixture.producer(),
            msg: vec![0xaa],
        };
        state
            .sandbox_apply(
                &matching,
                state.shared_state().clone(),
                state.local_state().clone(),
                &[Effect::Broadcast { data: vec![0xbb] }],
                None,
                None,
                None,
            )
            .unwrap();
        // The committed queue still holds the authored head until the step is
        // certified; the proposal carries the post-dispatch queue.
        assert_eq!(state.outgoing(), &[vec![0xaa]]);
        assert_eq!(
            state.pending_shared().expect("proposal").outgoing(),
            &[vec![0xbb]]
        );
        let commitment = state.proposal_commitment().unwrap().clone();
        for (peer, key) in &fixture.participants {
            state
                .add_step_signature(ParticipantStepSignature::new(
                    *peer,
                    commitment.step,
                    key.sign(&commitment.signing_bytes()),
                ))
                .unwrap();
        }
        assert!(state.pending_shared().is_none());
        assert_eq!(state.outgoing(), &[vec![0xbb]]);
        assert_valid(&state);
    }

    #[test]
    fn author_and_receiver_build_identical_entries_for_the_same_message() {
        let fixture = fixture();
        let author_peer = fixture.producer();
        let receiver_peer = fixture
            .participants
            .iter()
            .map(|(peer, _)| *peer)
            .find(|peer| *peer != author_peer)
            .expect("a remote participant");

        // The author queues the message, then applies its own message.
        let mut author = started_state(&fixture);
        author
            .sandbox_apply(
                &local_timer_event(),
                author.shared_state().clone(),
                author.local_state().clone(),
                &[Effect::Broadcast { data: vec![0xaa] }],
                None,
                None,
                None,
            )
            .unwrap();
        author
            .sandbox_apply(
                &Event::MessageReceived {
                    from: author_peer,
                    msg: vec![0xaa],
                },
                author.shared_state().clone(),
                author.local_state().clone(),
                &[],
                None,
                None,
                None,
            )
            .unwrap();

        // A different participant applies the same message from a different
        // local queue length. The entry must be byte-identical.
        let mut receiver = started_state_as(&fixture, receiver_peer);
        receiver
            .sandbox_apply(
                &local_timer_event(),
                receiver.shared_state().clone(),
                receiver.local_state().clone(),
                &[Effect::Broadcast { data: vec![0xcc] }],
                None,
                None,
                None,
            )
            .unwrap();
        receiver
            .sandbox_apply(
                &Event::MessageReceived {
                    from: author_peer,
                    msg: vec![0xaa],
                },
                receiver.shared_state().clone(),
                receiver.local_state().clone(),
                &[],
                None,
                None,
                None,
            )
            .unwrap();

        let authored = author.pending_shared().expect("author proposal");
        let received = receiver.pending_shared().expect("receiver proposal");
        assert_eq!(
            author.proposal_commitment().expect("author commitment"),
            receiver.proposal_commitment().expect("receiver commitment")
        );
        assert_eq!(
            borsh::to_vec(authored.entry()).expect("encode author entry"),
            borsh::to_vec(received.entry()).expect("encode receiver entry"),
        );
    }

    #[test]
    fn drop_outgoing_head_removes_only_the_oldest_message() {
        let fixture = fixture();
        let mut state = active_state(&fixture);
        state
            .sandbox_apply(
                &local_timer_event(),
                state.shared_state().clone(),
                state.local_state().clone(),
                &[
                    Effect::Broadcast { data: vec![1] },
                    Effect::Broadcast { data: vec![2] },
                ],
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(state.outgoing(), &[vec![1], vec![2]]);
        state.drop_outgoing_head().unwrap();
        assert_valid(&state);
        assert_eq!(state.outgoing(), &[vec![2]]);
        state.drop_outgoing_head().unwrap();
        assert_eq!(
            state.drop_outgoing_head(),
            Err(ProtocolError::NotQueuedMessage)
        );
    }

    #[test]
    fn effect_budget_is_exact_at_the_aggregate_bound() {
        let broadcast = |bytes: usize| Effect::Broadcast {
            data: vec![0u8; bytes],
        };
        // The aggregate counts the canonical `Vec<Effect>` length prefix, so 63
        // maximal broadcasts fit and 64 do not.
        let fits: Vec<Effect> = (0..63).map(|_| broadcast(64 * 1024)).collect();
        assert!(crate::execution::check_effect_budget(fits.iter()).is_ok());
        let over: Vec<Effect> = (0..64).map(|_| broadcast(64 * 1024)).collect();
        assert!(matches!(
            crate::execution::check_effect_budget(over.iter()),
            Err(ProtocolError::EncodedTooLarge { .. })
        ));
        // One byte over the per-effect payload bound is rejected at emission.
        assert!(matches!(
            crate::execution::check_effect_budget([&broadcast(64 * 1024 + 1)]),
            Err(ProtocolError::PayloadTooLarge { .. })
        ));
    }

    #[test]
    fn local_dispatch_with_changed_shared_image_is_rejected_without_mutation() {
        let fixture = fixture();
        let mut state = active_state(&fixture);
        let before = state.encode().expect("encode state");
        let result = state.sandbox_apply(
            &local_timer_event(),
            SharedStateBytes::try_new(vec![0x99]).expect("changed shared state"),
            state.local_state().clone(),
            &[],
            None,
            None,
            None,
        );
        assert_eq!(result, Err(ProtocolError::LocalSharedChange));
        assert_eq!(state.encode().expect("re-encode state"), before);
    }

    #[test]
    fn agreed_dispatch_over_the_effect_budget_is_rejected_without_mutation() {
        let fixture = fixture();
        let mut state = active_state(&fixture);
        let ensemble =
            Ensemble::from_peers(fixture.participants.iter().map(|(p, _)| *p).collect()).unwrap();
        // Each timer fits its individual bound; only the aggregate exceeds
        // the 4 MiB budget recovery enforces.
        let timers: Vec<Effect> = (0..64u64)
            .map(|delay_ms| Effect::SetTimer {
                delay_ms,
                timer: crate::TimerPayload {
                    type_name: "t".into(),
                    data: vec![0u8; 64 * 1024],
                },
            })
            .collect();
        let before = state.encode().expect("encode state");
        let result = state.sandbox_apply(
            &Event::SessionStarted { ensemble },
            state.shared_state().clone(),
            state.local_state().clone(),
            &timers,
            None,
            None,
            None,
        );
        assert!(
            matches!(result, Err(ProtocolError::EncodedTooLarge { .. })),
            "over-budget effects must not stage: {result:?}"
        );
        assert_eq!(state.encode().expect("re-encode state"), before);
    }

    #[test]
    fn outgoing_queue_bound_is_enforced_on_decode() {
        let fixture = fixture();
        let mut state = active_state(&fixture);
        state.outgoing = vec![Vec::new(); super::super::MAX_OUTGOING_MESSAGES + 1];
        let bytes = borsh::to_vec(&state).expect("encode");
        assert!(ExecutionState::decode(&bytes).is_err());
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
            .dispatch_event();
            if let Event::MessageReceived { msg, .. } = &mut event {
                *msg = vec![1; super::super::MAX_EFFECT_PAYLOAD_BYTES];
            }
            let before = state.clone();
            let result = state.sandbox_apply(
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
                // A terminal step is subject to the same check; it cannot
                // certify an outcome that leaves local assembly impossible.
                assert!(matches!(
                    state.sandbox_apply(
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
            let commitment = state.proposal_commitment().unwrap().clone();
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
            assert_valid(&state);
        }
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

        // Step 0: the session boundary, with no effects.
        state
            .sandbox_apply(
                &Event::SessionStarted { ensemble },
                state.shared_state().clone(),
                state.local_state().clone(),
                &[],
                None,
                None,
                None,
            )
            .unwrap();
        certify_pending(&mut state, &fixture, &mut trace);

        // Step 1: an agreed message that ends the session. The broadcast it
        // emits is queued, not staged as an effect.
        let remote = fixture
            .participants
            .iter()
            .map(|(peer, _)| *peer)
            .find(|peer| *peer != fixture.producer())
            .unwrap();
        let msg = vec![1u8];
        let event = Event::MessageReceived { from: remote, msg };
        state
            .sandbox_apply(
                &event,
                state.shared_state().clone(),
                state.local_state().clone(),
                &[Effect::SessionEnd { outcome: vec![7] }],
                Some(outcome.clone()),
                None,
                None,
            )
            .unwrap();
        // Even an unchanged shared image requires agreement for SessionEnd.
        assert_eq!(state.status(), &ExecutionStatus::Active);
        certify_pending(&mut state, &fixture, &mut trace);
        assert_eq!(
            state.status(),
            &ExecutionStatus::Certified {
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
        stopped.last_mut().unwrap().terminal = Some(StepTerminal::Abort {
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
        assert_valid(&state);
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
            .sandbox_apply(
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
        let commitment = state.proposal_commitment().expect("staged commitment");
        for (peer, key) in &fixture.participants {
            state
                .add_step_signature(ParticipantStepSignature::new(
                    *peer,
                    commitment.step,
                    key.sign(&commitment.signing_bytes()),
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
        .dispatch_event();
        state
            .sandbox_apply(
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
        let commitment = state.proposal_commitment().expect("staged commitment");
        for (peer, key) in &fixture.participants {
            state
                .add_step_signature(ParticipantStepSignature::new(
                    *peer,
                    commitment.step,
                    key.sign(&commitment.signing_bytes()),
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
            .stage_proposal(proposal)
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
                StateHash::of_shared(&fixture.initial),
                Vec::new(),
                ExecutionStatus::active(),
                Some(pending),
            )
            .expect("dispatch installation");
        assert_valid(&state);
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
                StateHash::of_shared(&fixture.initial),
                Vec::new(),
                ExecutionStatus::active(),
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
        state.stage_proposal(proposal).expect("stage proposal");
        assert_valid(&state);
        let commitment = state.proposal_commitment().expect("staged commitment");
        let (signer, signer_key) = fixture
            .participants
            .iter()
            .find(|(peer, _)| (*peer == fixture.producer()) == signer_is_producer)
            .expect("signer key");
        let signature = ParticipantStepSignature::new(
            *signer,
            commitment.step,
            signer_key.sign(&commitment.signing_bytes()),
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
            assert_valid(&state);
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
        malformed.entry.post_state = StateHash([0xee; 32]);
        let before = state.clone();
        assert!(state.stage_proposal(malformed).is_err());
        assert_eq!(state, before);

        let proposal = proposal_for(&fixture, &state, proposed_shared, proposed_local);
        state
            .stage_proposal(proposal.clone())
            .expect("valid proposal");
        assert_valid(&state);
        assert_eq!(state.event_position(), 1);
        assert_eq!(state.pending_shared(), Some(&proposal));

        let after_stage = state.clone();
        assert_eq!(
            state.stage_proposal(proposal).unwrap_err(),
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
            .stage_proposal(proposal_for(
                &fixture,
                &state,
                proposed_shared.clone(),
                proposed_local.clone(),
            ))
            .expect("valid proposal");
        let staged = state.pending_shared().expect("staged proposal").clone();
        // The staged commitment derives from the pre-commit link; keep it
        // for the assertions below since certification advances the link.
        let staged_commitment = state.proposal_commitment().expect("staged commitment");

        let (first_peer, first_key) = &fixture.participants[0];
        let first_signature = ParticipantStepSignature::new(
            *first_peer,
            staged_commitment.step,
            first_key.sign(&staged_commitment.signing_bytes()),
        );
        assert!(
            state
                .add_step_signature(first_signature)
                .expect("first signature")
                .is_none()
        );
        assert_valid(&state);
        assert_eq!(state.shared_state(), &fixture.initial);
        assert_eq!(state.local_state().as_bytes(), &[0x90]);

        let (second_peer, second_key) = &fixture.participants[1];
        let second_signature = ParticipantStepSignature::new(
            *second_peer,
            staged_commitment.step,
            second_key.sign(&staged_commitment.signing_bytes()),
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
        assert_eq!(state.agreed_link(), staged_commitment.link_hash());
        assert_valid(&state);
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
            .stage_proposal(proposal_for(
                &fixture,
                &state,
                second_shared.clone(),
                second_local.clone(),
            ))
            .expect("second proposal");
        assert_valid(&state);
        let commitment = state.proposal_commitment().expect("staged commitment");
        for (peer, key) in &fixture.participants {
            let signature = ParticipantStepSignature::new(
                *peer,
                commitment.step,
                key.sign(&commitment.signing_bytes()),
            );
            state
                .add_step_signature(signature)
                .expect("reaction signature");
        }
        assert_valid(&state);
        assert_eq!(state.agreed_step(), 2);
        assert_eq!(state.shared_state(), &second_shared);
        assert_eq!(state.local_state(), &second_local);
    }

    #[test]
    fn terminal_proposal_validates_with_a_queued_broadcast() {
        let fixture = fixture();
        let state = active_state(&fixture);
        let terminal = Effect::SessionAbort {
            reason: "guest stopped".into(),
        };

        let mut proposal = proposal_for(
            &fixture,
            &state,
            SharedStateBytes::try_new(vec![0xa1]).expect("state"),
            LocalStateBytes::try_new(vec![0xa2]).expect("state"),
        );
        proposal.entry.terminal = Some(
            StepTerminal::from_effect(&terminal).expect("lifecycle effect is a terminal value"),
        );
        proposal.effects = vec![(0, terminal)];
        proposal.outgoing = vec![vec![0xa3]];
        // The commitment derives from the mutated entry; no stored copy is
        // rebuilt here.
        proposal.status = ExecutionStatus::from_shared_entry(
            &proposal.entry,
            StepCommitment::for_entry(
                fixture.activation.session_hash(),
                &proposal.entry,
                state.agreed_link(),
            ),
        )
        .expect("terminal status construction")
        .expect("abort status");
        validate_proposal(
            state.binding(),
            state.step_cursor(),
            state.event_position(),
            &proposal,
        )
        .expect("a terminal proposal may retain a queued broadcast");
    }

    #[test]
    fn incomplete_and_conflicting_certificates_do_not_mutate_state() {
        let fixture = fixture();
        let mut state = active_state(&fixture);
        let staged_shared = SharedStateBytes::try_new(vec![0x81]).expect("state");
        state
            .stage_proposal(proposal_for(
                &fixture,
                &state,
                staged_shared,
                LocalStateBytes::try_new(vec![0x82]).expect("state"),
            ))
            .expect("valid proposal");
        assert_valid(&state);
        let commitment = state.proposal_commitment().expect("staged commitment");
        let (peer, key) = &fixture.participants[0];
        let signature = ParticipantStepSignature::new(
            *peer,
            commitment.step,
            key.sign(&commitment.signing_bytes()),
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
        let conflicting = ParticipantStepSignature::new(
            *peer,
            state.proposal_commitment().expect("staged commitment").step,
            BlsSignature([0; 48]),
        );
        assert_eq!(
            state.add_step_signature(conflicting).unwrap_err(),
            ProtocolError::ConflictingStepSignature { participant: *peer }
        );

        let incomplete = StepCertificate::from_signatures(
            state.binding(),
            &state.proposal_commitment().expect("staged commitment"),
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
            commitment: state.proposal_commitment().expect("staged commitment"),
            agreement: AggregateAttestation::empty(),
        };
        assert!(matches!(
            state.certify_step(malformed),
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
            ProtocolError::StateHashMismatch
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
        terminal.entry.terminal = Some(StepTerminal::Abort {
            reason: "invalid status".into(),
        });
        terminal.effects = terminal
            .entry
            .terminal
            .clone()
            .map(|terminal| (0, terminal.to_effect()))
            .into_iter()
            .collect();
        // The commitment derives from the mutated entry; no stored copy is
        // rebuilt here.
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
