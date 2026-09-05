use arena0_program::{LocalStateBytes, SharedStateBytes};
use borsh::{BorshDeserialize, BorshSerialize};
use serde::de::Error as _;
use serde::{Deserialize, Serialize};
#[cfg(feature = "performance-tracing")]
use std::time::Instant;

use crate::exec::ExecLifecycle;
use crate::negotiation::Activation;
use crate::trace::{
    AggregateAttestation, PrivateRecord, StepCommitment, TerminalCommitment, TraceEntry,
};
use crate::{ExecId, PeerId, StateHash};

use super::{
    ActiveTimer, ExecutionBinding, ExecutionStatus, ExecutionVersion, MAX_ACTIVE_TIMERS,
    MAX_EXECUTION_STATE_BYTES, ParticipantStepSignature, PrivateCursor, ProtocolError,
    PublicCursor, ReceiptArtifact, ReceiptId, TerminalOutcome, TerminalProof, TimerId,
    ensure_encoded, validate_proposal,
};
/// The pending public proposal retained in the durable aggregate.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SharedProposal {
    pub(crate) commitment: StepCommitment,
    pub(crate) entry: TraceEntry,
    pub(crate) shared_state: SharedStateBytes,
    pub(crate) terminal_outcome: Option<TerminalOutcome>,
    pub(crate) signatures: Vec<ParticipantStepSignature>,
}

impl SharedProposal {
    /// Borrow the proposed trace entry.
    #[must_use]
    pub const fn entry(&self) -> &TraceEntry {
        &self.entry
    }

    /// Borrow the proposed shared state.
    #[must_use]
    pub const fn shared_state(&self) -> &SharedStateBytes {
        &self.shared_state
    }

    /// Borrow the complete terminal projection paired with a successful
    /// terminal proposal, if present.
    #[must_use]
    pub const fn terminal_outcome(&self) -> Option<&TerminalOutcome> {
        self.terminal_outcome.as_ref()
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

/// A public commit materialized by a complete step certificate.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SharedCommit {
    pub(crate) entry: TraceEntry,
    pub(crate) shared_state: SharedStateBytes,
    pub(crate) terminal_outcome: Option<TerminalOutcome>,
    pub(crate) certificate: StepCertificate,
}

impl SharedCommit {
    /// Borrow the committed public trace entry.
    #[must_use]
    pub const fn entry(&self) -> &TraceEntry {
        &self.entry
    }

    /// Borrow the committed shared state.
    #[must_use]
    pub const fn shared_state(&self) -> &SharedStateBytes {
        &self.shared_state
    }

    /// Borrow the terminal projection committed with this step, if any.
    #[must_use]
    pub const fn terminal_outcome(&self) -> Option<&TerminalOutcome> {
        self.terminal_outcome.as_ref()
    }

    /// Borrow its activation-bound certificate.
    #[must_use]
    pub const fn certificate(&self) -> &StepCertificate {
        &self.certificate
    }
}

/// A private commit materialized by a private delta.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PrivateCommit {
    pub(crate) record: PrivateRecord,
    pub(crate) local_state: LocalStateBytes,
}

/// The private coordinate that created the stored continuation. It is kept
/// beside the public `PendingRecord` so recovery can re-derive its id instead
/// of trusting an opaque caller-supplied number.
#[derive(
    BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq,
)]
pub struct PendingCoordinate {
    pub(crate) record: u64,
    pub(crate) effect_index: u32,
}

impl PendingCoordinate {
    /// Return the private-record sequence that created the continuation.
    #[must_use]
    pub const fn record(&self) -> u64 {
        self.record
    }

    /// Return the effect ordinal that created the continuation.
    #[must_use]
    pub const fn effect_index(&self) -> u32 {
        self.effect_index
    }
}

impl PrivateCommit {
    /// Borrow the private record.
    #[must_use]
    pub const fn record(&self) -> &PrivateRecord {
        &self.record
    }

    /// Borrow the replacement private state.
    #[must_use]
    pub const fn local_state(&self) -> &LocalStateBytes {
        &self.local_state
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

/// A terminal publication commit.  Its effect is emitted only when the
/// terminal certificate is full and the authenticated artifact is present and valid.
#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct TerminalPublication {
    pub(crate) receipt: ReceiptArtifact,
}

impl TerminalPublication {
    /// Borrow the authenticated receipt artifact.
    #[must_use]
    pub const fn receipt(&self) -> &ReceiptArtifact {
        &self.receipt
    }
}

/// The one durable execution aggregate.  Fields are private so callers cannot
/// forge a cursor, pending proof, or lifecycle combination. Recovery must use
/// [`ExecutionState::decode`], which checks aggregate invariants after every
/// nested value has passed its own validating decoder.
#[derive(BorshSerialize, Serialize, Debug, Clone, PartialEq)]
pub struct ExecutionState {
    pub(crate) execution_id: ExecId,
    pub(crate) binding: ExecutionBinding,
    pub(crate) producer: PeerId,
    pub(crate) status: ExecutionStatus,
    pub(crate) version: ExecutionVersion,
    pub(crate) public: PublicCursor,
    pub(crate) private: PrivateCursor,
    pub(crate) shared_state: SharedStateBytes,
    pub(crate) local_state: LocalStateBytes,
    pub(crate) proposal: Option<SharedProposal>,
    pub(crate) timers: Vec<ActiveTimer>,
}

#[derive(BorshSerialize, BorshDeserialize, Serialize, Deserialize)]
struct ExecutionStateBody {
    pub(crate) execution_id: ExecId,
    pub(crate) binding: ExecutionBinding,
    pub(crate) producer: PeerId,
    pub(crate) status: ExecutionStatus,
    pub(crate) version: ExecutionVersion,
    pub(crate) public: PublicCursor,
    pub(crate) private: PrivateCursor,
    pub(crate) shared_state: SharedStateBytes,
    pub(crate) local_state: LocalStateBytes,
    pub(crate) proposal: Option<SharedProposal>,
    pub(crate) timers: Vec<ActiveTimer>,
}

impl ExecutionState {
    fn body(&self) -> ExecutionStateBody {
        ExecutionStateBody {
            execution_id: self.execution_id,
            binding: self.binding.clone(),
            producer: self.producer,
            status: self.status.clone(),
            version: self.version,
            public: self.public,
            private: self.private,
            shared_state: self.shared_state.clone(),
            local_state: self.local_state.clone(),
            proposal: self.proposal.clone(),
            timers: self.timers.clone(),
        }
    }

    fn from_body(body: ExecutionStateBody) -> Result<Self, ProtocolError> {
        let state = Self {
            execution_id: body.execution_id,
            binding: body.binding,
            producer: body.producer,
            status: body.status,
            version: body.version,
            public: body.public,
            private: body.private,
            shared_state: body.shared_state,
            local_state: body.local_state,
            proposal: body.proposal,
            timers: body.timers,
        };
        state.validate_recovered()?;
        Ok(state)
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
    /// Construct the initial aggregate.  Activation is already validated and
    /// bound, the producer must be one of its participants, and lifecycle is
    /// always `Activating` until the activation input.
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
        if StateHash::of(shared_state.as_bytes()) != binding.activation.offer().data().initial_state
        {
            return Err(ProtocolError::PublicStateHashMismatch);
        }
        let public = PublicCursor::genesis(&shared_state);
        Ok(Self {
            execution_id,
            binding,
            producer,
            status: ExecutionStatus::Activating,
            version: ExecutionVersion::ZERO,
            public,
            private: PrivateCursor::default(),
            shared_state,
            local_state,
            proposal: None,
            timers: Vec::new(),
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

    /// Return the local Host identity owning this execution.
    #[must_use]
    pub const fn producer(&self) -> PeerId {
        self.producer
    }

    /// Return the public lifecycle projection.
    #[must_use]
    pub const fn lifecycle(&self) -> ExecLifecycle {
        self.status.lifecycle()
    }

    /// Borrow the single durable execution status owner.
    #[must_use]
    pub const fn status(&self) -> &ExecutionStatus {
        &self.status
    }

    /// Return the identities retained by a published terminal status.
    ///
    /// The store uses this projection while checking that its terminal proof,
    /// receipt, and proof-artifact rows agree with the aggregate. Unpublished
    /// statuses return `None` and therefore must not have publication rows.
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

    pub(crate) fn terminal_proof(&self) -> Option<&TerminalProof> {
        self.status.terminal_proof()
    }

    /// Return the durable execution version.
    #[must_use]
    pub const fn version(&self) -> ExecutionVersion {
        self.version
    }

    /// Return the public consensus cursor.
    #[must_use]
    pub const fn public(&self) -> PublicCursor {
        self.public
    }

    /// Return the local private cursor.
    #[must_use]
    pub const fn private(&self) -> PrivateCursor {
        self.private
    }

    /// Borrow the committed opaque shared state.
    #[must_use]
    pub const fn shared_state(&self) -> &SharedStateBytes {
        &self.shared_state
    }

    /// Borrow the committed opaque local state.
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

    /// Borrow the guest-produced terminal outcome projection once the
    /// successful terminal boundary has been committed.
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

    /// Borrow active timer identities for recovery.
    #[must_use = "inspect the active timer identities"]
    pub fn active_timers(&self) -> impl Iterator<Item = TimerId> + '_ {
        self.timers.iter().map(|timer| timer.id)
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
                    target: "arena0::performance",
                    operation = "execution_state_decode",
                    exec_id = %state.execution_id(),
                    session_id = %state.binding().session_id(),
                    version = state.version().get(),
                    public_step = state.public().next_step(),
                    encoded_size,
                    success = true,
                    result_class = "decoded",
                    elapsed_us,
                ),
                Err(_) => tracing::debug!(
                    target: "arena0::performance",
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
        let bytes = borsh::to_vec(&self.body())
            .map_err(|error| ProtocolError::Serialization(error.to_string()))?;
        ensure_encoded("execution state", bytes.len(), MAX_EXECUTION_STATE_BYTES)?;
        Ok(bytes)
    }

    /// Validate aggregate cross-field invariants after decoding persisted
    /// bytes. Nested values, including the activation and its signatures,
    /// validate in their own decoders before this method runs.
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
        if StateHash::of(self.shared_state.as_bytes()) != self.public.state_hash {
            return Err(ProtocolError::PublicStateHashMismatch);
        }
        if self.public.next_step == 0
            && self.public.state_hash != self.binding.activation.offer().data().initial_state
        {
            return Err(ProtocolError::PublicStateHashMismatch);
        }
        if self
            .private
            .last_reaction_position()
            .is_some_and(|position| {
                self.private.next_record() == 0 || position > self.public.next_step
            })
        {
            return Err(ProtocolError::InvalidPrivateReactionCursor);
        }
        self.status.validate_binding(
            self.execution_id,
            &self.binding,
            self.public,
            self.private,
        )?;
        match &self.status {
            ExecutionStatus::Activating if self.proposal.is_some() => {
                return Err(ProtocolError::InvalidCertificate(
                    "activating execution cannot retain a shared proposal".into(),
                ));
            }
            ExecutionStatus::Waiting { .. }
            | ExecutionStatus::TerminalProof { .. }
            | ExecutionStatus::Completed { .. }
            | ExecutionStatus::Stopped { .. }
            | ExecutionStatus::StoppedPublished { .. }
            | ExecutionStatus::Incomplete { .. }
                if self.proposal.is_some() =>
            {
                return Err(ProtocolError::InvalidCertificate(
                    "status cannot retain a shared proposal".into(),
                ));
            }
            _ => {}
        }
        if self.timers.len() > MAX_ACTIVE_TIMERS {
            return Err(ProtocolError::TooManyTimers {
                actual: self.timers.len(),
                max: MAX_ACTIVE_TIMERS,
            });
        }
        for pair in self.timers.windows(2) {
            if pair[0].id >= pair[1].id {
                return Err(ProtocolError::TimerSetNotCanonical);
            }
        }
        if let Some(proposal) = &self.proposal {
            validate_proposal(&self.binding, self.public, proposal)?;
        }
        Ok(())
    }
}
